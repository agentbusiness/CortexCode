//! HTTP for every source and for fetch: one client (rustls, HTTP/2, the system proxy, compressed
//! responses), a descriptive User-Agent, size limits, retries of rate-limited requests when the
//! server asks for a short wait, arXiv's request spacing, and errors that say what happened in
//! words (`offline`, `rate limited`, `blocked`, an HTTP status).

use crate::config::Config;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum NetError {
    /// The host could not be reached at all (no network, DNS failure, connection refused).
    Unreachable(String),
    Timeout(String),
    /// 429 (or 403 with a rate-limit header) after the retries allowed.
    RateLimited(String),
    /// A bot wall, a 403 or a CAPTCHA page.
    Blocked(String),
    Status(u16, String),
    /// Refused before sending: a private address, a scheme other than http(s).
    Refused(String),
    Other(String),
}

impl NetError {
    pub fn message(&self) -> String {
        match self {
            Self::Unreachable(detail) => format!("unreachable: {detail}"),
            Self::Timeout(detail) => format!("timed out: {detail}"),
            Self::RateLimited(detail) => format!("rate limited: {detail}"),
            Self::Blocked(detail) => format!("blocked: {detail}"),
            Self::Status(status, detail) => format!("HTTP {status}: {detail}"),
            Self::Refused(detail) => format!("refused: {detail}"),
            Self::Other(detail) => detail.clone(),
        }
    }

    /// The network itself may be missing (as opposed to a server answering badly).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Timeout(_))
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// After redirects.
    pub url: String,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body was cut at the size limit.
    pub truncated: bool,
    pub headers: HashMap<String, String>,
}

impl Response {
    pub fn text(&self) -> String {
        decode_text(&self.body, &self.content_type)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// One request.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Most bytes kept (default: the configured download limit).
    pub max_bytes: Option<u64>,
    /// Refuse private and loopback hosts unless configured otherwise (URLs from users or models).
    pub guard: bool,
    /// Keep 4xx/5xx responses as responses instead of errors (sources that read error bodies).
    pub keep_errors: bool,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Self::default() }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }
}

pub struct Net {
    client: reqwest::Client,
    config: Config,
    /// Host -> the earliest time its next request may start (only hosts with a spacing rule).
    next_slot: Mutex<HashMap<String, Instant>>,
}

impl Net {
    pub fn new(config: Config) -> Self {
        let client = client_builder(&config)
            .build()
            .expect("valid research HTTP configuration");
        Self {
            client,
            config,
            next_slot: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Wait for the host's next slot when it has a spacing rule (arXiv: one request every 3 s, as its
    /// API terms ask).
    async fn space(&self, url: &url::Url) {
        let interval = match url.host_str() {
            Some(host) if host.ends_with("arxiv.org") || self.config.arxiv_api.contains(&format!("//{host}")) => {
                self.config.arxiv_interval_ms
            }
            _ => 0,
        };
        if interval == 0 {
            return;
        }
        let host = url.host_str().unwrap_or_default().to_owned();
        let wait = {
            let Ok(mut slots) = self.next_slot.lock() else {
                return;
            };
            let now = Instant::now();
            let start = slots.get(&host).copied().filter(|slot| *slot > now).unwrap_or(now);
            slots.insert(host, start + Duration::from_millis(interval));
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn get(&self, request: Request) -> Result<Response, NetError> {
        let mut url = url::Url::parse(&request.url).map_err(|error| {
            NetError::Refused(format!("`{}` is not a URL ({error})", request.url))
        })?;
        let original_origin = url.origin();
        let mut redirects = 0;
        let mut attempt = 0;
        loop {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(NetError::Refused(format!(
                    "only http and https URLs are fetched, not {}:",
                    url.scheme()
                )));
            }
            // Configured source endpoints are trusted for their first hop. Every fetch and every
            // redirect is resolved once, checked, then connected to exactly those addresses.
            let guarded = (request.guard || redirects > 0) && !self.config.allow_private_hosts;
            let client = if guarded {
                let addresses = public_addresses(&url).await?;
                client_builder(&self.config)
                    // A proxy may resolve the target itself, bypassing our checked DNS answers.
                    .no_proxy()
                    .resolve_to_addrs(url.host_str().unwrap_or_default(), &addresses)
                    .build()
                    .map_err(|error| {
                        NetError::Other(format!("cannot create the guarded HTTP client: {error}"))
                    })?
            } else {
                self.client.clone()
            };
            attempt += 1;
            self.space(&url).await;
            let mut builder = client.get(url.clone());
            // A source credential belongs to its original origin, never a redirect's new host.
            if url.origin() == original_origin {
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
            }
            let response = builder
                .send()
                .await
                .map_err(|error| classify(&error, &url))?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
                && let Some(location) = response.headers().get(reqwest::header::LOCATION)
            {
                if redirects >= 8 {
                    return Err(NetError::Refused("more than 8 redirects".into()));
                }
                let location = location.to_str().map_err(|_| {
                    NetError::Refused("redirect has an invalid Location header".into())
                })?;
                url = url
                    .join(location)
                    .map_err(|error| NetError::Refused(format!("invalid redirect: {error}")))?;
                redirects += 1;
                attempt = 0;
                continue;
            }
            let status = response.status().as_u16();
            let headers: HashMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(name, value)| Some((name.as_str().to_ascii_lowercase(), value.to_str().ok()?.to_owned())))
                .collect();
            if (status == 429 || status == 503) && attempt <= 2 {
                let wait = retry_wait(&headers);
                if wait <= Duration::from_millis(self.config.max_retry_wait_ms) {
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            let final_url = response.url().to_string();
            let content_type = headers.get("content-type").cloned().unwrap_or_default();
            let limit = request.max_bytes.unwrap_or(self.config.max_download_bytes);
            let (body, truncated) = read_limited(response, limit).await.map_err(|error| classify(&error, &url))?;
            let host = url.host_str().unwrap_or_default().to_owned();
            if status >= 400 && !request.keep_errors {
                return Err(status_error(status, &host, &headers, &body));
            }
            return Ok(Response { status, url: final_url, content_type, body, truncated, headers });
        }
    }
}

fn client_builder(config: &Config) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(config.user_agent.clone())
        .timeout(Duration::from_millis(config.timeout_ms.max(1_000)))
        .connect_timeout(Duration::from_millis(
            config.timeout_ms.clamp(1_000, 10_000),
        ))
        .redirect(reqwest::redirect::Policy::none())
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = (limit as usize).saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

fn retry_wait(headers: &HashMap<String, String>) -> Duration {
    let seconds = headers
        .get("retry-after")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .or_else(|| headers.get("x-ratelimit-reset").and_then(|value| value.trim().parse::<f64>().ok()).filter(|reset| *reset < 600.0));
    match seconds {
        Some(seconds) => Duration::from_millis((seconds.max(0.0) * 1000.0) as u64 + 250),
        None => Duration::from_millis(1_500),
    }
}

/// The error for a 4xx/5xx answer, in words.
pub fn status_error(status: u16, host: &str, headers: &HashMap<String, String>, body: &[u8]) -> NetError {
    let snippet = crate::text::clip(&crate::text::strip_tags(&String::from_utf8_lossy(&body[..body.len().min(2_000)])), 160);
    let remaining_zero = headers.get("x-ratelimit-remaining").is_some_and(|value| value.trim().trim_end_matches(".0") == "0");
    if status == 429 || (status == 403 && remaining_zero) {
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|reset| *reset > 1_000_000_000)
            .and_then(|reset| chrono::DateTime::from_timestamp(reset, 0))
            .map(|time| format!("; resets at {}", time.format("%H:%M:%S UTC")))
            .unwrap_or_default();
        return NetError::RateLimited(format!("{host} answered {status}{reset} ({snippet})"));
    }
    if status == 403 || status == 451 {
        return NetError::Blocked(format!("{host} refused the request with {status} ({snippet})"));
    }
    NetError::Status(status, format!("{host}: {snippet}"))
}

fn classify(error: &reqwest::Error, url: &url::Url) -> NetError {
    let host = url.host_str().unwrap_or_default();
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        chain.push_str(": ");
        chain.push_str(&inner.to_string());
        source = inner.source();
    }
    if error.is_timeout() {
        return NetError::Timeout(format!("{host} did not answer in time"));
    }
    if error.is_connect() {
        let lower = chain.to_lowercase();
        let why = if lower.contains("dns") || lower.contains("lookup") || lower.contains("nodename") {
            "cannot resolve the host name"
        } else if lower.contains("refused") {
            "connection refused"
        } else if lower.contains("unreachable") {
            "the network is unreachable"
        } else {
            "cannot connect"
        };
        return NetError::Unreachable(format!("{host}: {why}"));
    }
    if error.is_redirect() {
        return NetError::Refused(format!("{host}: {chain}"));
    }
    NetError::Other(format!("{host}: {chain}"))
}

/// A host name that is a loopback, private, link-local or unspecified address literal, or a local
/// name.
pub fn literal_private(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(address) = bare.parse::<IpAddr>() {
        return private_address(&address);
    }
    let lower = bare.to_ascii_lowercase();
    lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") || lower.ends_with(".internal")
}

pub fn private_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| private_address(&IpAddr::V4(v4)))
        }
    }
}

async fn public_addresses(url: &url::Url) -> Result<Vec<SocketAddr>, NetError> {
    let host = url.host_str().unwrap_or_default();
    if literal_private(host) {
        return Err(NetError::Refused(format!("{host} is a local or private address (set allow_private_hosts to fetch it)")));
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<_> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|error| NetError::Unreachable(format!("cannot resolve {host}: {error}")))?
        .collect();
    if addresses.is_empty() {
        return Err(NetError::Unreachable(format!("{host} has no addresses")));
    }
    if let Some(address) = addresses
        .iter()
        .find(|address| private_address(&address.ip()))
    {
        return Err(NetError::Refused(format!(
            "{host} resolves to the private address {}",
            address.ip()
        )));
    }
    Ok(addresses)
}

/// Decode a body as text: the charset of the Content-Type, else a `<meta charset>` in the first
/// kilobytes, else UTF-8 (lossy).
pub fn decode_text(body: &[u8], content_type: &str) -> String {
    let declared = charset_of(content_type).or_else(|| {
        let head = String::from_utf8_lossy(&body[..body.len().min(4_096)]).to_ascii_lowercase();
        let at = head.find("charset=")?;
        let rest = head[at + 8..].trim_start_matches(['"', '\'']);
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    });
    let encoding = declared.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

fn charset_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let at = lower.find("charset=")?;
    Some(lower[at + 8..].trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace()).split(';').next()?.to_owned())
}
