//! gopls, delve and the Go toolchain they need.
//!
//! **Install** (`lang-go.install`) puts everything into `$CORTEX_HOME/toolchains`, never anywhere
//! system-wide:
//! - Go [`GO_VERSION`]: the official release archive for darwin (go.dev/dl, SHA-256 pinned here)
//!   unpacked into `toolchains/go/<version>/go`;
//! - gopls [`GOPLS_VERSION`] and delve [`DLV_VERSION`]: built by that Go with `go install
//!   <package>@v<version>` into `toolchains/gopls/<version>/gopls` and `toolchains/dlv/<version>/dlv`
//!   (GOBIN is a staging directory renamed into place; GOPATH, the module cache and the build cache
//!   are `toolchains/gopath`; `GOTOOLCHAIN=local`, so the pinned Go never fetches another).
//!
//! **Locate**: gopls installed there, else one on PATH, in `$GOBIN`, `$GOPATH/bin` or `~/go/bin`,
//! accepted only when `gopls version` runs; gopls also needs a `go` command (it runs `go list`), on
//! PATH or the installed one: without either the server is reported unavailable, saying so.
//!
//! **Toolchain** (`describe`'s `path` / `env`, prepended to every command's PATH by the tools): the
//! installed Go's `bin` when no runnable `go` is on PATH (then also `GOTOOLCHAIN=local`), and the
//! gopls and delve directories when they are installed.

use cc_lang_kit::install::{ArchiveFormat, FoundServer, Release, Toolchains, find_existing, find_in, is_executable, probe_version, run_logged};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "gopls";
/// The Go release installed (go.dev/dl, stable, 2026-10).
pub const GO_VERSION: &str = "1.27.2";
/// SHA-256 of `go1.27.2.darwin-arm64.tar.gz` and `go1.27.2.darwin-amd64.tar.gz` (go.dev/dl/?mode=json).
const GO_SHA256_DARWIN_ARM64: &str = "76812b213b1b2302c978d28fa52fa92d541704b9e7d9d5db8002c50e4018c4c5";
const GO_SHA256_DARWIN_AMD64: &str = "587b59182488b23aa6e5fc25110405a3e0e5b38ed2f5b2f46ed13c32aee356fe";
pub const GOPLS_VERSION: &str = "0.23.0";
pub const GOPLS_PACKAGE: &str = "golang.org/x/tools/gopls";
pub const DLV_VERSION: &str = "1.27.2";
pub const DLV_PACKAGE: &str = "github.com/go-delve/delve/cmd/dlv";

/// The process environment locating reads, explicit so tests can give their own.
#[derive(Debug, Clone, Default)]
pub struct Host {
    /// `PATH`.
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    pub gobin: Option<PathBuf>,
    /// `GOPATH` (a path list; its first entry holds `bin`).
    pub gopath: Option<OsString>,
}

impl Host {
    pub fn current() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        Self { path: var("PATH"), home: var("HOME").map(PathBuf::from), gobin: var("GOBIN").map(PathBuf::from), gopath: var("GOPATH") }
    }
}

/// The pinned Go release for this machine.
pub fn go_release() -> Result<Release, String> {
    let (arch, sha256) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", GO_SHA256_DARWIN_ARM64),
        "x86_64" => ("amd64", GO_SHA256_DARWIN_AMD64),
        other => return Err(format!("no pinned Go release for {other}")),
    };
    if std::env::consts::OS != "macos" {
        return Err(format!("no pinned Go release for {}", std::env::consts::OS));
    }
    Ok(Release {
        version: GO_VERSION.into(),
        url: format!("https://go.dev/dl/go{GO_VERSION}.darwin-{arch}.tar.gz"),
        sha256: sha256.into(),
        format: ArchiveFormat::TarGz,
    })
}

/// `toolchains/go/<GO_VERSION>/go/bin` (the archive's top directory is `go`).
pub fn go_bin(toolchains: &Toolchains) -> Result<PathBuf, String> {
    Ok(toolchains.version_dir("go", GO_VERSION)?.join("go").join("bin"))
}

/// `toolchains/<name>/<version>`, the GOBIN of an installed Go tool.
pub fn tool_dir(toolchains: &Toolchains, name: &str, version: &str) -> Result<PathBuf, String> {
    toolchains.version_dir(name, version)
}

/// A runnable `go`: on PATH first (the user's own), else the installed one.
pub fn find_go(toolchains: &Toolchains, host: &Host) -> Result<FoundServer, String> {
    let mut candidates = find_in("go", host.path.clone());
    candidates.push(go_bin(toolchains)?.join("go"));
    find_existing(&candidates, &["version".into()], None, &[("GOTOOLCHAIN", "local")])
}

/// Whether a runnable `go` is on PATH (then the system's Go serves commands).
fn system_go(host: &Host) -> bool {
    find_existing(&find_in("go", host.path.clone()), &["version".into()], None, &[("GOTOOLCHAIN", "local")]).is_ok()
}

pub fn spec(toolchains: &Toolchains, host: &Host) -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.version_args = vec!["version".into()];
    spec.language_ids = vec![("go".into(), "go".into())];
    // gopls checks the module and workspace files too (by name: any other `*.mod` is not one).
    spec.file_names = vec![("go.mod".into(), "go.mod".into()), ("go.work".into(), "go.work".into())];
    // gopls runs `go list`: the user's `go` on PATH first, the installed one after it.
    let mut path: Vec<PathBuf> = host.path.as_ref().map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    if let Ok(bin) = go_bin(toolchains) {
        path.push(bin);
    }
    if let Ok(joined) = std::env::join_paths(path) {
        spec.env.push(("PATH".into(), joined.to_string_lossy().into_owned()));
    }
    // Never download another Go toolchain behind the user's back (a go.mod `toolchain` line).
    spec.env.push(("GOTOOLCHAIN".into(), "local".into()));
    // Hover text without pkg.go.dev links (the model reads it, it does not browse it).
    let settings = json!({"linksInHover": false});
    spec.initialization_options = settings.clone();
    spec.settings = json!({"gopls": settings});
    // Loading a module (go list, the type checker over dependencies) takes a while the first time.
    spec.diagnostics_wait = Duration::from_secs(90);
    spec
}

/// A runnable gopls (and a Go for it), for `root` or the plugin's working directory.
pub fn locate(toolchains: &Toolchains, host: &Host, root: Option<&Path>) -> Result<FoundServer, String> {
    let mut candidates = vec![tool_dir(toolchains, NAME, GOPLS_VERSION)?.join(NAME)];
    candidates.extend(find_in(NAME, host.path.clone()));
    if let Some(gobin) = &host.gobin {
        candidates.push(gobin.join(NAME));
    }
    if let Some(first) = host.gopath.as_ref().and_then(|p| std::env::split_paths(p).next()) {
        candidates.push(first.join("bin").join(NAME));
    }
    if let Some(home) = &host.home {
        candidates.push(home.join("go").join("bin").join(NAME));
    }
    let install_hint = format!("`lang-go.install` installs Go {GO_VERSION}, gopls {GOPLS_VERSION} and delve {DLV_VERSION} into {}", toolchains.root().display());
    let found = match find_existing(&candidates, &["version".into()], root, &[("GOTOOLCHAIN", "local")]) {
        Ok(found) => found,
        Err(problem) if problem == "no executable found" => return Err(format!("{NAME} is not installed; {install_hint}")),
        Err(problem) => return Err(format!("{NAME} is not installed: {problem}; {install_hint}")),
    };
    match find_go(toolchains, host) {
        Ok(_) => Ok(found),
        Err(problem) => {
            let why = if problem == "no executable found" { "no `go` on PATH or in the toolchains directory".to_owned() } else { problem };
            Err(format!("{NAME} ({}) at {} needs a Go toolchain: {why}; {install_hint}", found.version, found.path.display()))
        }
    }
}

/// The environment of `go install` for a tool: everything it writes stays in the toolchains
/// directory (`GOBIN` the staging directory, GOPATH / module cache / build cache under
/// `toolchains/gopath`), and the pinned Go never switches to another toolchain.
pub fn install_env(toolchains: &Toolchains, gobin: &Path) -> Vec<(String, String)> {
    let gopath = toolchains.root().join("gopath");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    vec![
        ("GOBIN".into(), s(gobin)),
        ("GOPATH".into(), s(&gopath)),
        ("GOMODCACHE".into(), s(&gopath.join("pkg").join("mod"))),
        ("GOCACHE".into(), s(&gopath.join("cache"))),
        ("GOTOOLCHAIN".into(), "local".into()),
        // Module cache files writable, so the toolchains directory can be deleted.
        ("GOFLAGS".into(), "-modcacherw".into()),
    ]
}

/// `go install <package>@v<version>` into `toolchains/<name>/<version>/<name>`, staged in a
/// sibling directory and renamed into place. An install that runs already is kept.
pub fn go_install(toolchains: &Toolchains, go: &Path, name: &str, package: &str, version: &str, log: &mut Vec<String>) -> Result<PathBuf, String> {
    let target = tool_dir(toolchains, name, version)?;
    let exe = target.join(name);
    if is_executable(&exe) {
        log.push(format!("{name} {version} is already installed in {}", target.display()));
        return Ok(exe);
    }
    let parent = toolchains.dir(name)?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let staging = parent.join(format!(".install-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let env = install_env(toolchains, &staging);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let outcome = run_logged(go, &["install".into(), format!("{package}@v{version}")], Some(&staging), &env, Duration::from_secs(1800), log).and_then(|_| {
        if is_executable(&staging.join(name)) { Ok(()) } else { Err(format!("`go install {package}@v{version}` finished but {name} is not in {}", staging.display())) }
    });
    if let Err(error) = outcome {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
    }
    std::fs::rename(&staging, &target).map_err(|e| format!("cannot move {} to {}: {e}", staging.display(), target.display()))?;
    log.push(format!("installed {name} {version} into {}", target.display()));
    Ok(exe)
}

/// Install Go, gopls and delve into the toolchains directory.
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    let release = go_release()?;
    let go_dir = toolchains.download_release("go", &release, log)?;
    let go = go_dir.join("go").join("bin").join("go");
    let version = probe_version(&go, &["version".into()], None, &[("GOTOOLCHAIN", "local")]).map_err(|e| format!("the installed Go does not run: {e}"))?;
    log.push(version);
    go_install(toolchains, &go, NAME, GOPLS_PACKAGE, GOPLS_VERSION, log)?;
    go_install(toolchains, &go, "dlv", DLV_PACKAGE, DLV_VERSION, log)?;
    Ok(())
}

/// What commands need: the installed Go when the system has none, and gopls / delve when installed.
pub fn toolchain(toolchains: &Toolchains, host: &Host) -> Toolchain {
    let mut out = Toolchain::default();
    if let Ok(bin) = go_bin(toolchains)
        && is_executable(&bin.join("go"))
        && !system_go(host)
    {
        out.path.push(bin.to_string_lossy().into_owned());
        out.env.push(("GOTOOLCHAIN".into(), "local".into()));
    }
    for (name, version) in [(NAME, GOPLS_VERSION), ("dlv", DLV_VERSION)] {
        if let Ok(dir) = tool_dir(toolchains, name, version)
            && is_executable(&dir.join(name))
        {
            out.path.push(dir.to_string_lossy().into_owned());
        }
    }
    out
}
