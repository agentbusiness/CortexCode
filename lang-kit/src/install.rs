//! Toolchains: language servers and tools a language plugin installs, each in
//! `$CORTEX_HOME/toolchains/<name>/<version>/`, or finds already present (an executable on PATH or
//! at a location the language's own tooling uses).
//!
//! Installation never reaches outside the toolchains directory: an npm package is installed with
//! `npm install --prefix <dir>` (never globally), a release archive is downloaded over HTTPS, checked
//! against its pinned SHA-256 and unpacked there. Homebrew, the system Python and global npm are
//! never touched. Installs are staged in a sibling directory and renamed into place, so a version
//! directory is either complete or absent.
//!
//! These functions block (they run processes and downloads); call them from blocking threads
//! (`tokio::task::spawn_blocking`), which is how the pack runtime calls a pack's locate and install.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a `--version` style probe of an executable may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// An executable that runs, and what it says its version is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundServer {
    pub path: PathBuf,
    pub version: String,
}

/// CortexCode's home: `CORTEX_HOME` when set and non-empty, else `~/.cortexcode`.
pub fn cortex_home() -> PathBuf {
    home_from(std::env::var_os("CORTEX_HOME"), std::env::var_os("HOME"))
}

/// [`cortex_home`] from the two variables' values.
pub fn home_from(cortex_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match cortex_home.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(home.unwrap_or_default()).join(".cortexcode"),
    }
}

/// A toolchain or version name: one path segment of letters, digits, `.`, `_`, `+` and `-`, not
/// starting with `.` (no hidden, parent or nested directories).
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// An archive's layout.
#[derive(Debug, Clone, PartialEq)]
pub enum ArchiveFormat {
    /// A gzip-compressed tar (`.tar.gz`, `.tgz`).
    TarGz,
    /// A zip archive.
    Zip,
    /// One gzip-compressed file (`rust-analyzer-aarch64-apple-darwin.gz`), installed as `file_name`.
    Gzip { file_name: String },
    /// The executable itself, installed as `file_name`.
    Binary { file_name: String },
}

/// A pinned release: where it is, what its bytes hash to and how it is packed.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// `https://` only.
    pub url: String,
    /// Lowercase hex SHA-256 of the downloaded file.
    pub sha256: String,
    pub format: ArchiveFormat,
}

/// `$CORTEX_HOME/toolchains`.
#[derive(Debug, Clone)]
pub struct Toolchains {
    root: PathBuf,
}

impl Toolchains {
    pub fn new(home: &Path) -> Self {
        Self { root: home.join("toolchains") }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `toolchains/<name>`.
    pub fn dir(&self, name: &str) -> Result<PathBuf, String> {
        if !valid_segment(name) {
            return Err(format!("`{name}` is not a toolchain name"));
        }
        Ok(self.root.join(name))
    }

    /// `toolchains/<name>/<version>`.
    pub fn version_dir(&self, name: &str, version: &str) -> Result<PathBuf, String> {
        if !valid_segment(version) {
            return Err(format!("`{version}` is not a version"));
        }
        Ok(self.dir(name)?.join(version))
    }

    /// A fresh sibling directory of `toolchains/<name>/<version>` to stage an install in.
    fn staging(&self, name: &str, label: &str) -> Result<PathBuf, String> {
        let dir = self.dir(name)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Ok(dir.join(format!(".{label}-{}-{nanos}", std::process::id())))
    }

    /// Move a finished staging directory into place as `toolchains/<name>/<version>`.
    fn commit(&self, staging: &Path, target: &Path, log: &mut Vec<String>) -> Result<(), String> {
        if target.exists() {
            std::fs::remove_dir_all(target).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
        }
        std::fs::rename(staging, target).map_err(|e| format!("cannot move {} to {}: {e}", staging.display(), target.display()))?;
        log.push(format!("installed into {}", target.display()));
        Ok(())
    }

    /// Install the npm package `package@version` into `toolchains/<name>/<version>` with
    /// `npm install --prefix`; returns its `node_modules/.bin` directory. An install of the same
    /// version that is already complete is kept.
    pub fn npm_install(&self, name: &str, package: &str, version: &str, log: &mut Vec<String>) -> Result<PathBuf, String> {
        let target = self.version_dir(name, version)?;
        let bin = target.join("node_modules").join(".bin");
        if npm_installed_version(&target, package).as_deref() == Some(version) {
            log.push(format!("{package}@{version} is already installed in {}", target.display()));
            return Ok(bin);
        }
        let npm = find_on_path("npm").into_iter().next().ok_or_else(|| format!("npm is not installed; Node.js and npm are needed to install {package}"))?;
        let staging = self.staging(name, "npm")?;
        std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
        let args = npm_install_args(&staging, package, version)?;
        let outcome = run_logged(&npm, &args, Some(&staging), &[], Duration::from_secs(900), log);
        let installed = outcome.and_then(|_| match npm_installed_version(&staging, package) {
            Some(found) if found == version => Ok(()),
            Some(found) => Err(format!("npm installed {package}@{found}, not {version}")),
            None => Err(format!("npm finished but {package} is not in {}", staging.display())),
        });
        if let Err(error) = installed {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        self.commit(&staging, &target, log)?;
        Ok(bin)
    }

    /// Download `release` and install it into `toolchains/<name>/<version>`; returns that
    /// directory. An install of the same version that is already present is kept.
    pub fn download_release(&self, name: &str, release: &Release, log: &mut Vec<String>) -> Result<PathBuf, String> {
        let target = self.version_dir(name, &release.version)?;
        if target.is_dir() {
            log.push(format!("{name} {} is already installed in {}", release.version, target.display()));
            return Ok(target);
        }
        if !release.url.starts_with("https://") {
            return Err(format!("refusing to download {} (https only)", release.url));
        }
        let file = self.staging(name, "download")?;
        log.push(format!("downloading {}", release.url));
        let outcome = download(&release.url, &file).and_then(|bytes| {
            log.push(format!("downloaded {bytes} bytes"));
            self.install_release_file(name, release, &file, log)
        });
        let _ = std::fs::remove_file(&file);
        outcome
    }

    /// Install `release` from the already downloaded `file`: check its SHA-256, unpack it into a
    /// staging directory and move that into `toolchains/<name>/<version>`.
    pub fn install_release_file(&self, name: &str, release: &Release, file: &Path, log: &mut Vec<String>) -> Result<PathBuf, String> {
        let target = self.version_dir(name, &release.version)?;
        let actual = sha256_file(file)?;
        if !actual.eq_ignore_ascii_case(release.sha256.trim()) {
            return Err(format!("checksum mismatch for {}: expected {}, got {actual}", release.url, release.sha256));
        }
        log.push(format!("sha256 {actual} verified"));
        let staging = self.staging(name, "unpack")?;
        std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
        if let Err(error) = unpack(file, &release.format, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        self.commit(&staging, &target, log)?;
        Ok(target)
    }
}

/// The arguments of `npm install --prefix <dir> <package>@<version>` (validated: a package name
/// and an exact version, nothing npm would read as an option or a URL).
pub fn npm_install_args(dir: &Path, package: &str, version: &str) -> Result<Vec<String>, String> {
    let package_ok = !package.is_empty()
        && !package.starts_with(['-', '.'])
        && package.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '@' | '/' | '-' | '.' | '_' | '~'));
    if !package_ok {
        return Err(format!("`{package}` is not an npm package name"));
    }
    if !valid_segment(version) {
        return Err(format!("`{version}` is not an exact version"));
    }
    Ok(vec![
        "install".into(),
        "--prefix".into(),
        dir.to_string_lossy().into_owned(),
        "--no-audit".into(),
        "--no-fund".into(),
        format!("{package}@{version}"),
    ])
}

/// The version of `package` installed under `prefix/node_modules`, from its package.json.
pub fn npm_installed_version(prefix: &Path, package: &str) -> Option<String> {
    let text = std::fs::read_to_string(prefix.join("node_modules").join(package).join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("version").and_then(|v| v.as_str()).map(str::to_owned)
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Run `future` to completion from a blocking thread: on the current runtime's handle when there is
/// one (a `spawn_blocking` thread), else on a runtime of its own.
fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("cannot start a runtime: {e}"))?;
            Ok(runtime.block_on(future))
        }
    }
}

/// Download `url` into `file`; returns the byte count.
fn download(url: &str, file: &Path) -> Result<u64, String> {
    block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .user_agent(concat!("CortexCode-lang-kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("cannot create the HTTP client: {e}"))?;
        let mut response = client.get(url).send().await.map_err(|e| format!("cannot download {url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("cannot download {url}: HTTP {}", response.status()));
        }
        let mut out = std::fs::File::create(file).map_err(|e| format!("cannot create {}: {e}", file.display()))?;
        let mut total = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| format!("download of {url} failed: {e}"))? {
            out.write_all(&chunk).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            total += chunk.len() as u64;
        }
        out.flush().map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        Ok(total)
    })?
}

fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("cannot chmod {}: {e}", path.display()))
}

/// Unpack `file` (laid out as `format`) into the empty directory `into`. Entries whose path would
/// leave `into` (absolute, `..`) fail the whole unpack.
pub fn unpack(file: &Path, format: &ArchiveFormat, into: &Path) -> Result<(), String> {
    let open = || std::fs::File::open(file).map_err(|e| format!("cannot open {}: {e}", file.display()));
    match format {
        ArchiveFormat::TarGz => {
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(open()?));
            archive.set_preserve_permissions(true);
            let entries = archive.entries().map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            for entry in entries {
                let mut entry = entry.map_err(|e| format!("cannot read {}: {e}", file.display()))?;
                let path = entry.path().map_err(|e| e.to_string())?.into_owned();
                if !safe_relative(&path) {
                    return Err(format!("{} holds an unsafe path {}", file.display(), path.display()));
                }
                let unpacked = entry.unpack_in(into).map_err(|e| format!("cannot unpack {}: {e}", path.display()))?;
                if !unpacked {
                    return Err(format!("{} holds an unsafe path {}", file.display(), path.display()));
                }
            }
            Ok(())
        }
        ArchiveFormat::Zip => {
            let mut archive = zip::ZipArchive::new(open()?).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
                let Some(relative) = entry.enclosed_name() else {
                    return Err(format!("{} holds an unsafe path {}", file.display(), entry.name()));
                };
                let path = into.join(relative);
                if entry.is_dir() {
                    std::fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
                    continue;
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                let mut out = std::fs::File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
                std::io::copy(&mut entry, &mut out).map_err(|e| format!("cannot unpack {}: {e}", path.display()))?;
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777)).map_err(|e| format!("cannot chmod {}: {e}", path.display()))?;
                }
            }
            Ok(())
        }
        ArchiveFormat::Gzip { file_name } | ArchiveFormat::Binary { file_name } => {
            if !valid_segment(file_name) {
                return Err(format!("`{file_name}` is not a file name"));
            }
            let path = into.join(file_name);
            let mut out = std::fs::File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            let copied = if matches!(format, ArchiveFormat::Gzip { .. }) {
                std::io::copy(&mut flate2::read::GzDecoder::new(open()?), &mut out)
            } else {
                std::io::copy(&mut open()?, &mut out)
            };
            copied.map_err(|e| format!("cannot unpack {}: {e}", file.display()))?;
            make_executable(&path)
        }
    }
}

fn safe_relative(path: &Path) -> bool {
    path.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

/// A regular file with an execute bit.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Every executable called `name` on PATH, in PATH order.
pub fn find_on_path(name: &str) -> Vec<PathBuf> {
    find_in(name, std::env::var_os("PATH"))
}

/// Every executable called `name` in the directories of the PATH-style `path_var`, in order.
pub fn find_in(name: &str, path_var: Option<OsString>) -> Vec<PathBuf> {
    let Some(path_var) = path_var else { return Vec::new() };
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) && !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

/// `$CARGO_HOME` (default `~/.cargo`).
pub fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
}

/// Output of a finished command.
#[derive(Debug, Clone)]
pub struct Ran {
    pub success: bool,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program args` (in `cwd`, with `env` added to the environment) to completion within
/// `limit`; the process is killed when the limit passes.
pub fn run(program: &Path, args: &[String], cwd: Option<&Path>, env: &[(&str, &str)], limit: Duration) -> Result<Ran, String> {
    let shown = program.display().to_string();
    block_on(async {
        let mut command = tokio::process::Command::new(program);
        command.args(args).envs(env.iter().copied()).stdin(std::process::Stdio::null()).kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        match tokio::time::timeout(limit, command.output()).await {
            Err(_) => Err(format!("{shown} did not finish within {} s", limit.as_secs_f32())),
            Ok(Err(error)) => Err(format!("cannot run {shown}: {error}")),
            Ok(Ok(output)) => Ok(Ran {
                success: output.status.success(),
                status: output.status.to_string(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    })?
}

/// The last `count` non-empty lines of `text`, joined.
pub fn tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// [`run`] with the command, its outcome and the tail of its output appended to `log`; Ok(stdout)
/// when it succeeded.
pub fn run_logged(program: &Path, args: &[String], cwd: Option<&Path>, env: &[(&str, &str)], limit: Duration, log: &mut Vec<String>) -> Result<String, String> {
    log.push(format!("$ {} {}", program.display(), args.join(" ")));
    let ran = run(program, args, cwd, env, limit)?;
    let output = tail(&format!("{}\n{}", ran.stdout, ran.stderr), 20);
    if !output.is_empty() {
        log.push(output.clone());
    }
    if ran.success {
        Ok(ran.stdout)
    } else {
        log.push(format!("failed: {}", ran.status));
        Err(format!("{} {} failed ({}): {}", program.display(), args.join(" "), ran.status, tail(&ran.stderr, 5)))
    }
}

/// Run `exe version_args` and return the first line it prints (stdout, else stderr) when it
/// succeeds: the executable exists and runs.
pub fn probe_version(exe: &Path, version_args: &[String], cwd: Option<&Path>, env: &[(&str, &str)]) -> Result<String, String> {
    let ran = run(exe, version_args, cwd, env, PROBE_TIMEOUT)?;
    let first = |text: &str| text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_owned);
    if ran.success {
        first(&ran.stdout).or_else(|| first(&ran.stderr)).ok_or_else(|| format!("{} printed no version", exe.display()))
    } else {
        Err(format!("{} {} failed ({}): {}", exe.display(), version_args.join(" "), ran.status, tail(&ran.stderr, 3)))
    }
}

/// The first of `candidates` that is an executable and answers `version_args`; otherwise why each
/// one is unusable.
pub fn find_existing(candidates: &[PathBuf], version_args: &[String], cwd: Option<&Path>, env: &[(&str, &str)]) -> Result<FoundServer, String> {
    let mut problems: Vec<String> = Vec::new();
    let mut seen: Vec<&PathBuf> = Vec::new();
    for candidate in candidates {
        if seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate);
        if !is_executable(candidate) {
            continue;
        }
        match probe_version(candidate, version_args, cwd, env) {
            Ok(version) => return Ok(FoundServer { path: candidate.clone(), version }),
            Err(problem) => problems.push(problem),
        }
    }
    Err(if problems.is_empty() { "no executable found".to_owned() } else { problems.join("; ") })
}
