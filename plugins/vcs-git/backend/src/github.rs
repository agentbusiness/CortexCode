//! The GitHub connection, through GitHub's REST API with the user's own sign-in: the device flow
//! (a code the user approves in the browser) or a personal access token. The token lives only in
//! the macOS Keychain (on Linux: a file only the user can read, `github-token` in the plugin's data
//! folder); the app's main store keeps the account and the dev folder (`vcs.github`; the
//! `settings.json` it was kept in before is read once and removed). Nothing here contacts GitHub
//! unless a method asks for it.

use cc_protocol::contracts::vcs::{Credential, DeviceCode, GithubPr, GithubStatus, Repo, Repos};
use cc_protocol::{RpcError, codes};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const API: &str = "https://api.github.com";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const SCOPES: &str = "repo read:user";
const REPO_PAGES: u32 = 5;
const FORK_READY_TRIES: u32 = 30;
const FORK_READY_STEP: Duration = Duration::from_secs(2);

/// What is kept in the store: never the token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    dev_folder: Option<String>,
}

/// A device sign-in waiting for the user (in memory only).
struct Device {
    device_code: String,
    interval: Duration,
    next_poll: Instant,
    expires: Instant,
}

/// The account a token belongs to (`GET /user`).
struct Account {
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    scopes: Vec<String>,
}

/// A repository as the REST API returns it.
#[derive(Deserialize)]
struct ApiRepo {
    full_name: String,
    private: bool,
    #[serde(default)]
    description: Option<String>,
    clone_url: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    fork: bool,
}

impl From<ApiRepo> for Repo {
    fn from(repo: ApiRepo) -> Self {
        let updated_ms = repo
            .updated_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis().max(0) as u64)
            .unwrap_or(0);
        Repo {
            full_name: repo.full_name,
            private: repo.private,
            description: repo.description.unwrap_or_default(),
            clone_url: repo.clone_url,
            default_branch: repo.default_branch.unwrap_or_else(|| "main".into()),
            updated_ms,
            fork: repo.fork,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn not_connected() -> RpcError {
    RpcError::new(codes::UNAVAILABLE, "GitHub is not connected; connect it in Settings > GitHub")
}

fn unreachable(error: reqwest::Error) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, format!("GitHub could not be reached: {}", error.without_url())).retryable()
}

/// `owner/name`, checked so it can go into an API path as it is.
pub fn split_full_name(repo: &str) -> Result<(&str, &str), RpcError> {
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok((owner, name)),
        _ => Err(RpcError::invalid_params(format!("`{repo}` is not a repository name like owner/name"))),
    }
}

/// GitHub's error answer as one short line: the message, then the first detail it gives.
fn api_error(status: StatusCode, headers: &HeaderMap, body: &Value) -> RpcError {
    let message = body.get("message").and_then(Value::as_str).unwrap_or("").trim_end_matches('.');
    let detail = body
        .get("errors")
        .and_then(|errors| errors.get(0))
        .and_then(|first| first.get("message").or_else(|| first.get("code")))
        .and_then(Value::as_str);
    let text = match detail {
        Some(detail) if !message.is_empty() => format!("{message}: {detail}"),
        Some(detail) => detail.to_owned(),
        None => message.to_owned(),
    };
    let out_of_calls = headers.get("x-ratelimit-remaining").is_some_and(|left| left == "0");
    match status {
        StatusCode::UNAUTHORIZED => RpcError::new(
            codes::UNAVAILABLE,
            "GitHub no longer accepts the saved sign-in; connect again in Settings > GitHub",
        ),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS if out_of_calls || text.contains("rate limit") => {
            RpcError::new(codes::BUSY, "GitHub's rate limit is reached; try again in a few minutes").retryable()
        }
        StatusCode::FORBIDDEN => RpcError::invalid_params(format!("GitHub refused this: {text}")),
        StatusCode::NOT_FOUND => RpcError::not_found("GitHub has no such repository, or this account cannot see it"),
        StatusCode::UNPROCESSABLE_ENTITY => RpcError::invalid_params(format!("GitHub: {text}")),
        _ => RpcError::internal(format!("GitHub answered {}: {text}", status.as_u16())),
    }
}

async fn keychain<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, RpcError> {
    // Keychain calls may wait for the user (an access prompt): keep them off the async workers.
    tokio::task::spawn_blocking(work).await.map_err(|error| RpcError::internal(format!("keychain task: {error}")))
}

/// Where the token is kept: the macOS Keychain.
#[cfg(target_os = "macos")]
mod secret {
    use std::path::Path;

    pub const STORE: &str = "the Keychain";
    /// The Keychain item holding the token (a generic password).
    const SERVICE: &str = "CortexCode GitHub";
    const ACCOUNT: &str = "token";
    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn get(_dir: &Path) -> Result<Option<Vec<u8>>, String> {
        match security_framework::passwords::get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn set(_dir: &Path, secret: &[u8]) -> Result<(), String> {
        security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, secret).map_err(|error| error.to_string())
    }

    pub fn delete(_dir: &Path) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT) {
            Err(error) if error.code() != NOT_FOUND => Err(error.to_string()),
            _ => Ok(()),
        }
    }
}

/// Where the token is kept: `github-token` in the plugin's data folder, readable by the user only.
#[cfg(not(target_os = "macos"))]
mod secret {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub const STORE: &str = "the token file";
    const FILE: &str = "github-token";

    pub fn get(dir: &Path) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(dir.join(FILE)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn set(dir: &Path, secret: &[u8]) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        let path = dir.join(FILE);
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|error| error.to_string())?;
        file.write_all(secret).map_err(|error| error.to_string())
    }

    pub fn delete(dir: &Path) -> Result<(), String> {
        match std::fs::remove_file(dir.join(FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        }
    }
}

pub struct Github {
    http: reqwest::Client,
    settings_path: PathBuf,
    /// The plugin's data folder (the token file on Linux).
    data_dir: PathBuf,
    settings: cc_protocol::stored::Stored<Settings>,
    /// The GitHub app's client id for the device flow (`github_client_id`); empty: unavailable.
    client_id: Mutex<String>,
    device: Mutex<Option<Device>>,
    /// The token once read from the secret store (read once per process).
    token: Mutex<Option<String>>,
}

impl Github {
    pub fn new(data_dir: &Path) -> Result<Self, RpcError> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
        headers.insert("X-GitHub-Api-Version", HeaderValue::from_static("2022-11-28"));
        let http = reqwest::Client::builder()
            .user_agent(concat!("CortexCode/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| RpcError::internal(format!("HTTP client: {error}")))?;
        Ok(Self {
            http,
            settings_path: data_dir.join("settings.json"),
            data_dir: data_dir.to_path_buf(),
            settings: cc_protocol::stored::Stored::new("vcs.github"),
            client_id: Mutex::new(String::new()),
            device: Mutex::new(None),
            token: Mutex::new(None),
        })
    }

    pub fn set_client_id(&self, client_id: Option<&str>) {
        *lock(&self.client_id) = client_id.unwrap_or("").trim().to_owned();
    }

    pub fn status(&self) -> GithubStatus {
        let settings = self.settings.get();
        let pending = lock(&self.device).as_ref().is_some_and(|device| Instant::now() < device.expires);
        GithubStatus {
            connected: settings.login.is_some(),
            pending,
            login: settings.login,
            name: settings.name,
            avatar_url: settings.avatar_url,
            method: settings.method,
            scopes: settings.scopes,
            device_available: !lock(&self.client_id).is_empty(),
            dev_folder: settings.dev_folder,
            error: None,
        }
    }

    fn status_with_error(&self, error: &str) -> GithubStatus {
        GithubStatus { error: Some(error.to_owned()), ..self.status() }
    }

    pub fn dev_folder(&self) -> Option<PathBuf> {
        self.settings.get().dev_folder.as_ref().map(PathBuf::from)
    }

    /// Read the account and dev folder from the store (or once from the file they were kept in).
    pub async fn load(&self, ctx: &cc_protocol::Ctx) -> Result<(), RpcError> {
        self.settings.load(ctx, Some(self.settings_path.clone())).await
    }

    fn update(&self, change: impl FnOnce(&mut Settings)) -> Result<(), RpcError> {
        self.settings.update(change);
        Ok(())
    }

    pub fn dev_folder_set(&self, path: &str) -> Result<GithubStatus, RpcError> {
        let folder = Path::new(path.trim());
        if !folder.is_absolute() {
            return Err(RpcError::invalid_params(format!("the dev folder must be an absolute path, got `{path}`")));
        }
        std::fs::create_dir_all(folder)
            .map_err(|error| RpcError::invalid_params(format!("cannot create {}: {error}", folder.display())))?;
        let folder = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        self.update(|settings| settings.dev_folder = Some(folder.to_string_lossy().into_owned()))?;
        Ok(self.status())
    }

    /// The saved token: from memory, else from the secret store. A connection whose stored token is
    /// gone is forgotten.
    async fn token(&self) -> Result<String, RpcError> {
        if self.settings.get().login.is_none() {
            return Err(not_connected());
        }
        if let Some(token) = lock(&self.token).clone() {
            return Ok(token);
        }
        let dir = self.data_dir.clone();
        let stored = keychain(move || secret::get(&dir)).await?;
        match stored {
            Ok(Some(bytes)) => {
                let token = String::from_utf8(bytes).map_err(|_| not_connected())?;
                *lock(&self.token) = Some(token.clone());
                Ok(token)
            }
            Ok(None) => {
                self.update(|settings| {
                    let dev_folder = settings.dev_folder.take();
                    *settings = Settings { dev_folder, ..Settings::default() };
                })?;
                Err(not_connected())
            }
            Err(error) => Err(RpcError::new(
                codes::UNAVAILABLE,
                format!("{} did not give back the GitHub sign-in: {error}", secret::STORE),
            )),
        }
    }

    /// The token for a clone from GitHub, when connected.
    pub async fn clone_token(&self) -> Option<String> {
        self.token().await.ok()
    }

    pub async fn credential(&self) -> Result<Credential, RpcError> {
        let token = self.token().await?;
        let login = self.settings.get().login.clone().ok_or_else(not_connected)?;
        Ok(Credential { login, token })
    }

    /// The account behind `token`, with the scopes GitHub reports for it.
    async fn account(&self, token: &str) -> Result<Account, RpcError> {
        let response = self.http.get(format!("{API}/user")).bearer_auth(token).send().await.map_err(unreachable)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status == StatusCode::UNAUTHORIZED {
            return Err(RpcError::invalid_params("GitHub did not accept this token"));
        }
        if !status.is_success() {
            return Err(api_error(status, &headers, &body));
        }
        let text = |key: &str| body.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
        let scopes = headers
            .get("x-oauth-scopes")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(Account {
            login: text("login").ok_or_else(|| RpcError::internal("GitHub returned no login for this token"))?,
            name: text("name"),
            avatar_url: text("avatar_url"),
            scopes,
        })
    }

    /// Check `token`, keep it in the secret store and the account in the settings.
    async fn connect(&self, token: String, method: &str, granted: Vec<String>) -> Result<GithubStatus, RpcError> {
        let account = self.account(&token).await?;
        let (dir, value) = (self.data_dir.clone(), token.clone());
        keychain(move || secret::set(&dir, value.as_bytes()))
            .await?
            .map_err(|error| RpcError::internal(format!("{} did not keep the GitHub sign-in: {error}", secret::STORE)))?;
        *lock(&self.token) = Some(token);
        let scopes = if account.scopes.is_empty() { granted } else { account.scopes };
        self.update(|settings| {
            settings.login = Some(account.login);
            settings.name = account.name;
            settings.avatar_url = account.avatar_url;
            settings.method = Some(method.to_owned());
            settings.scopes = scopes;
        })?;
        Ok(self.status())
    }

    pub async fn token_set(&self, token: &str) -> Result<GithubStatus, RpcError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(RpcError::invalid_params("the token is empty"));
        }
        self.connect(token.to_owned(), "token", Vec::new()).await
    }

    pub async fn disconnect(&self) -> Result<GithubStatus, RpcError> {
        let dir = self.data_dir.clone();
        if let Err(error) = keychain(move || secret::delete(&dir)).await? {
            return Err(RpcError::internal(format!("{} did not remove the GitHub sign-in: {error}", secret::STORE)));
        }
        *lock(&self.token) = None;
        *lock(&self.device) = None;
        self.update(|settings| {
            let dev_folder = settings.dev_folder.take();
            *settings = Settings { dev_folder, ..Settings::default() };
        })?;
        Ok(self.status())
    }

    pub async fn device_start(&self) -> Result<DeviceCode, RpcError> {
        let client_id = lock(&self.client_id).clone();
        if client_id.is_empty() {
            return Err(RpcError::new(
                codes::UNAVAILABLE,
                "GitHub sign-in with a code is not set up in this build; use a personal access token",
            ));
        }
        let response = self
            .http
            .post(DEVICE_CODE_URL)
            .header(ACCEPT, "application/json")
            .form(&[("client_id", client_id.as_str()), ("scope", SCOPES)])
            .send()
            .await
            .map_err(unreachable)?;
        let body: Value = response.json().await.map_err(unreachable)?;
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        let number = |key: &str| body.get(key).and_then(Value::as_u64);
        let (Some(device_code), Some(user_code), Some(verification_uri)) =
            (text("device_code"), text("user_code"), text("verification_uri"))
        else {
            let why = text("error_description").or_else(|| text("error")).unwrap_or_else(|| "no code returned".into());
            return Err(RpcError::new(codes::UNAVAILABLE, format!("GitHub did not start the sign-in: {why}")));
        };
        let interval = number("interval").unwrap_or(5).max(1);
        let expires_in = number("expires_in").unwrap_or(900);
        let now = Instant::now();
        *lock(&self.device) = Some(Device {
            device_code,
            interval: Duration::from_secs(interval),
            next_poll: now + Duration::from_secs(interval),
            expires: now + Duration::from_secs(expires_in),
        });
        Ok(DeviceCode { user_code, verification_uri, expires_in, interval })
    }

    /// Ask GitHub once whether the user approved the code; calls closer together than GitHub's
    /// interval answer `pending` without asking.
    pub async fn device_poll(&self) -> Result<GithubStatus, RpcError> {
        let client_id = lock(&self.client_id).clone();
        // `None`: nothing to ask GitHub now (the status says why); the device lock is released
        // before the status reads it again.
        let due = {
            let mut slot = lock(&self.device);
            let now = Instant::now();
            match slot.as_mut() {
                Some(device) if now >= device.expires => {
                    *slot = None;
                    Err("The code expired; start the sign-in again")
                }
                Some(device) if now >= device.next_poll => {
                    device.next_poll = now + device.interval;
                    Ok(Some(device.device_code.clone()))
                }
                _ => Ok(None),
            }
        };
        let device_code = match due {
            Ok(Some(code)) => code,
            Ok(None) => return Ok(self.status()),
            Err(expired) => return Ok(self.status_with_error(expired)),
        };
        let response = self
            .http
            .post(ACCESS_TOKEN_URL)
            .header(ACCEPT, "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("device_code", device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(unreachable)?;
        let body: Value = response.json().await.map_err(unreachable)?;
        if let Some(token) = body.get("access_token").and_then(Value::as_str) {
            *lock(&self.device) = None;
            let granted = body
                .get("scope")
                .and_then(Value::as_str)
                .map(|scope| scope.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default();
            return self.connect(token.to_owned(), "device", granted).await;
        }
        let error = body.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            "authorization_pending" => Ok(self.status()),
            "slow_down" => {
                if let Some(device) = lock(&self.device).as_mut() {
                    let seconds = body.get("interval").and_then(Value::as_u64).unwrap_or(device.interval.as_secs() + 5);
                    device.interval = Duration::from_secs(seconds);
                    device.next_poll = Instant::now() + device.interval;
                }
                Ok(self.status())
            }
            _ => {
                *lock(&self.device) = None;
                Ok(self.status_with_error(match error {
                    "expired_token" => "The code expired; start the sign-in again",
                    "access_denied" => "The sign-in was cancelled on GitHub",
                    _ => "GitHub refused the sign-in; start it again",
                }))
            }
        }
    }

    /// One REST call with the saved token: the status, the headers and the JSON body.
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, HeaderMap, Value), RpcError> {
        let token = self.token().await?;
        let mut request = self.http.request(method, format!("{API}{path}")).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(unreachable)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.json().await.unwrap_or(Value::Null);
        Ok((status, headers, body))
    }

    /// One REST call whose failure is an error.
    async fn api<T: serde::de::DeserializeOwned>(&self, method: Method, path: &str, body: Option<Value>) -> Result<T, RpcError> {
        let (status, headers, body) = self.send(method, path, body).await?;
        if !status.is_success() {
            return Err(api_error(status, &headers, &body));
        }
        serde_json::from_value(body).map_err(|error| RpcError::internal(format!("GitHub's answer to {path}: {error}")))
    }

    /// The user's repositories (owned and collaborating), most recently updated first.
    pub async fn repos(&self, query: Option<&str>) -> Result<Repos, RpcError> {
        let query = query.map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
        let mut repos = Vec::new();
        for page in 1..=REPO_PAGES {
            let path = format!("/user/repos?affiliation=owner,collaborator&sort=updated&per_page=100&page={page}");
            let batch: Vec<ApiRepo> = self.api(Method::GET, &path, None).await?;
            let last = batch.len() < 100;
            repos.extend(batch.into_iter().map(Repo::from));
            if last {
                break;
            }
        }
        if let Some(query) = query {
            repos.retain(|repo| repo.full_name.to_lowercase().contains(&query));
        }
        Ok(Repos { repos })
    }

    pub async fn repo_create(&self, name: &str, private: bool, description: &str) -> Result<Repo, RpcError> {
        let body = json!({ "name": name.trim(), "private": private, "description": description });
        Ok(self.api::<ApiRepo>(Method::POST, "/user/repos", Some(body)).await?.into())
    }

    /// The user's fork of `repo`: GitHub returns the existing one when there is one, else makes it.
    /// A new fork is copied in the background; wait until its default branch is there.
    pub async fn fork(&self, repo: &str) -> Result<Repo, RpcError> {
        let (owner, name) = split_full_name(repo)?;
        let fork: Repo = self.api::<ApiRepo>(Method::POST, &format!("/repos/{owner}/{name}/forks"), Some(json!({}))).await?.into();
        let branch = format!("/repos/{}/branches/{}", fork.full_name, fork.default_branch);
        for _ in 0..FORK_READY_TRIES {
            let (status, _, _) = self.send(Method::GET, &branch, None).await?;
            if status.is_success() {
                return Ok(fork);
            }
            tokio::time::sleep(FORK_READY_STEP).await;
        }
        Err(RpcError::new(codes::TIMEOUT, format!("GitHub is still copying {}; try again in a minute", fork.full_name))
            .retryable())
    }

    pub async fn pr_create(&self, repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<GithubPr, RpcError> {
        let (owner, name) = split_full_name(repo)?;
        let request = json!({ "title": title, "head": head, "base": base, "body": body });
        let pr: Value = self.api(Method::POST, &format!("/repos/{owner}/{name}/pulls"), Some(request)).await?;
        Ok(pull_request(&pr))
    }

    pub async fn pr_state(&self, repo: &str, number: u64) -> Result<GithubPr, RpcError> {
        let (owner, name) = split_full_name(repo)?;
        let pr: Value = self.api(Method::GET, &format!("/repos/{owner}/{name}/pulls/{number}"), None).await?;
        Ok(pull_request(&pr))
    }
}

fn pull_request(pr: &Value) -> GithubPr {
    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false) || pr.get("merged_at").is_some_and(|at| !at.is_null());
    GithubPr {
        url: pr.get("html_url").and_then(Value::as_str).unwrap_or_default().to_owned(),
        number: pr.get("number").and_then(Value::as_u64).unwrap_or(0),
        state: if merged { "merged".into() } else { pr.get("state").and_then(Value::as_str).unwrap_or("open").to_owned() },
    }
}
