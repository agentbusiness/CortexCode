//! Eclipse JDT LS (jdtls), the JDK it runs on, and the JDK commands need.
//!
//! **Java detection** never trusts `/usr/bin/java`: on macOS it is a stub that only works when a
//! JDK is installed in /Library/Java/JavaVirtualMachines. A system JDK is `$JAVA_HOME` when its
//! `bin/java -version` runs, else what `/usr/libexec/java_home` names (it fails when there is no
//! JDK: the stub case), else a `java` on PATH other than the stub whose real location is inside a
//! JDK. Each is accepted only when `java -version` runs; its version is read from that output.
//!
//! **Install** (`lang-java.install`), everything under `$CORTEX_HOME/toolchains`:
//! - a JDK, when no JDK 21+ is present: Eclipse Temurin [`TEMURIN_VERSION`] (the Adoptium release
//!   archive for macOS, SHA-256 pinned here) into `toolchains/jdk/<version>`;
//! - jdtls [`JDTLS_VERSION`]: the milestone archive from download.eclipse.org (SHA-256 pinned) into
//!   `toolchains/jdtls/<version>`.
//!
//! **Running jdtls**: the server is the JDK's `java` ([`locate`]); jdtls needs a per-workspace
//! data directory (`-data`) and its launcher jar's versioned name, so the server of each root starts
//! ([`launch`], the pack's `server_launch`) as `java ... -jar <launcher> ... -data
//! $CORTEX_HOME/cache/jdtls/<workspace>-<hash>` in the workspace root, the arguments jdtls's own
//! `bin/jdtls` script passes (the shared configuration area read-only, so the install directory is
//! never written). A configured `server_path` (a `jdtls` script) gets only `-data <dir>`.
//!
//! **Toolchain** (`describe`'s `path` / `env`): the installed JDK's `bin` and `JAVA_HOME` when the
//! system has no JDK of its own (Gradle wrappers and Maven need `JAVA_HOME`).

use cc_lang_kit::install::{ArchiveFormat, FoundServer, Release, Toolchains, find_in, is_executable, run};
use cc_lang_kit::lsp::{Launch, ServerSpec};
use cc_lang_kit::pack::Toolchain;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "jdtls";
/// The Java version jdtls needs.
pub const MIN_JAVA: u32 = 21;

pub const JDTLS_VERSION: &str = "1.61.0";
const JDTLS_BUILD: &str = "202609031315";
/// SHA-256 of jdt-language-server-1.61.0-202609031315.tar.gz (its published `.sha256`).
const JDTLS_SHA256: &str = "338e7e73d61836651ba2453919a0d34fa763eb4e7c03342092309bffb8934c64";

/// Eclipse Temurin 21 LTS (Adoptium, 2026-08).
pub const TEMURIN_VERSION: &str = "21.0.12.1+1";
const TEMURIN_SHA256_AARCH64: &str = "3623232f33a9c3baadf304480b2535f9a3cba8a58d42ecbb438ba267315d9998";
const TEMURIN_SHA256_X64: &str = "44db0f08196daf19a47f90d13388b0c943b67663cb537f998fe29e836fa842ce";

/// The process environment Java detection reads, explicit so tests can give their own.
#[derive(Debug, Clone)]
pub struct Host {
    pub path: Option<OsString>,
    pub java_home: Option<PathBuf>,
    /// macOS's JDK locator (`/usr/libexec/java_home`).
    pub java_home_tool: PathBuf,
}

impl Host {
    pub fn current() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        Self { path: var("PATH"), java_home: var("JAVA_HOME").map(PathBuf::from), java_home_tool: PathBuf::from("/usr/libexec/java_home") }
    }
}

/// A JDK that runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Jdk {
    /// JAVA_HOME.
    pub home: PathBuf,
    /// `java -version`'s version (`21.0.12`, `1.8.0_412`).
    pub version: String,
    /// The feature release (`21`, `8`).
    pub major: u32,
}

impl Jdk {
    pub fn java(&self) -> PathBuf {
        self.home.join("bin").join("java")
    }
}

/// The version in `java -version` output (`openjdk version "21.0.12" 2026-07-15`, `java version
/// "1.8.0_412"`) and its feature release (`1.x` is the old scheme for `x`).
pub fn parse_java_version(output: &str) -> Option<(String, u32)> {
    let line = output.lines().find(|l| l.contains(" version \""))?;
    let version = line.split('"').nth(1)?.to_owned();
    let mut numbers = version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().ok());
    let first = numbers.next()??;
    let major = if first == 1 { numbers.next()?? } else { first };
    Some((version, major))
}

/// `home/bin/java -version`, as a [`Jdk`].
pub fn probe_jdk(home: &Path) -> Result<Jdk, String> {
    let java = home.join("bin").join("java");
    if !is_executable(&java) {
        return Err(format!("{} is not an executable", java.display()));
    }
    let ran = run(&java, &["-version".into()], None, &[], Duration::from_secs(30))?;
    let output = format!("{}\n{}", ran.stderr, ran.stdout);
    if !ran.success {
        return Err(format!("{} -version failed ({}): {}", java.display(), ran.status, cc_lang_kit::install::tail(&output, 2)));
    }
    let (version, major) = parse_java_version(&output).ok_or_else(|| format!("{} -version printed no version", java.display()))?;
    Ok(Jdk { home: home.to_path_buf(), version, major })
}

/// The system's JDK (see the module documentation), with why each place had none.
pub fn system_jdk(host: &Host) -> Result<Jdk, String> {
    let mut problems = Vec::new();
    if let Some(home) = &host.java_home {
        match probe_jdk(home) {
            Ok(jdk) => return Ok(jdk),
            Err(problem) => problems.push(format!("JAVA_HOME: {problem}")),
        }
    }
    if is_executable(&host.java_home_tool) {
        match run(&host.java_home_tool, &[], None, &[], Duration::from_secs(30)) {
            Ok(ran) if ran.success => match probe_jdk(Path::new(ran.stdout.trim())) {
                Ok(jdk) => return Ok(jdk),
                Err(problem) => problems.push(format!("{}: {problem}", host.java_home_tool.display())),
            },
            Ok(_) => problems.push(format!("{} finds no JDK (/usr/bin/java is only the macOS stub)", host.java_home_tool.display())),
            Err(problem) => problems.push(problem),
        }
    }
    for java in find_in("java", host.path.clone()) {
        if java == Path::new("/usr/bin/java") {
            continue;
        }
        // A JDK's java lives in <home>/bin (Homebrew links it from elsewhere).
        let Some(home) = std::fs::canonicalize(&java).ok().and_then(|real| real.parent()?.parent().map(Path::to_path_buf)) else { continue };
        match probe_jdk(&home) {
            Ok(jdk) => return Ok(jdk),
            Err(problem) => problems.push(problem),
        }
    }
    Err(if problems.is_empty() { "no JDK found".to_owned() } else { format!("no JDK found ({})", problems.join("; ")) })
}

/// The pinned Temurin release for this machine.
pub fn temurin_release() -> Result<Release, String> {
    let (arch, sha256) = match std::env::consts::ARCH {
        "aarch64" => ("aarch64", TEMURIN_SHA256_AARCH64),
        "x86_64" => ("x64", TEMURIN_SHA256_X64),
        other => return Err(format!("no pinned JDK for {other}")),
    };
    let file_version = TEMURIN_VERSION.replace('+', "_");
    Ok(Release {
        version: TEMURIN_VERSION.into(),
        url: format!("https://github.com/adoptium/temurin21-binaries/releases/download/jdk-{}/OpenJDK21U-jdk_{arch}_mac_hotspot_{file_version}.tar.gz", TEMURIN_VERSION.replace('+', "%2B")),
        sha256: sha256.into(),
        format: ArchiveFormat::TarGz,
    })
}

pub fn jdtls_release() -> Release {
    Release {
        version: JDTLS_VERSION.into(),
        url: format!("https://download.eclipse.org/jdtls/milestones/{JDTLS_VERSION}/jdt-language-server-{JDTLS_VERSION}-{JDTLS_BUILD}.tar.gz"),
        sha256: JDTLS_SHA256.into(),
        format: ArchiveFormat::TarGz,
    }
}

/// The JAVA_HOME inside an unpacked JDK archive (`jdk-<version>/Contents/Home` on macOS).
pub fn jdk_home_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .flat_map(|top| [top.join("Contents").join("Home"), top])
        .filter(|home| is_executable(&home.join("bin").join("java")))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The installed Temurin, when it runs.
pub fn installed_jdk(toolchains: &Toolchains) -> Result<Jdk, String> {
    let dir = toolchains.version_dir("jdk", TEMURIN_VERSION)?;
    let home = jdk_home_in(&dir).ok_or_else(|| format!("no JDK installed in {}", dir.display()))?;
    probe_jdk(&home)
}

/// The JDK jdtls runs on: the installed Temurin, else a system JDK of version 21 or later.
pub fn jdk_for_jdtls(toolchains: &Toolchains, host: &Host) -> Result<Jdk, String> {
    if let Ok(jdk) = installed_jdk(toolchains) {
        return Ok(jdk);
    }
    match system_jdk(host) {
        Ok(jdk) if jdk.major >= MIN_JAVA => Ok(jdk),
        Ok(jdk) => Err(format!("the system's JDK is Java {} ({}), jdtls needs Java {MIN_JAVA} or later", jdk.version, jdk.home.display())),
        Err(problem) => Err(problem),
    }
}

/// An installed jdtls: its directory, launcher jar and the configuration directory for this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Jdtls {
    pub dir: PathBuf,
    pub launcher: PathBuf,
    pub configuration: PathBuf,
}

pub fn installed_jdtls(toolchains: &Toolchains) -> Result<Jdtls, String> {
    let dir = toolchains.version_dir(NAME, JDTLS_VERSION)?;
    let plugins = dir.join("plugins");
    let mut launchers: Vec<PathBuf> = std::fs::read_dir(&plugins)
        .map_err(|_| format!("jdtls {JDTLS_VERSION} is not installed ({} is missing)", plugins.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("org.eclipse.equinox.launcher_") && n.ends_with(".jar")))
        .collect();
    launchers.sort();
    let launcher = launchers.pop().ok_or_else(|| format!("jdtls in {} has no launcher jar", dir.display()))?;
    let config_name = if std::env::consts::ARCH == "aarch64" { "config_mac_arm" } else { "config_mac" };
    let configuration = dir.join(config_name);
    if !configuration.is_dir() {
        return Err(format!("jdtls in {} has no {config_name}", dir.display()));
    }
    Ok(Jdtls { dir, launcher, configuration })
}

/// The workspace data directory of a root: `$CORTEX_HOME/cache/jdtls/<name>-<hash>`.
pub fn data_dir(home: &Path, root: &Path) -> PathBuf {
    let name: String = root.file_name().map(|n| n.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect()).unwrap_or_default();
    let hash = cc_lang_kit::syntax::hash_hex(root.to_string_lossy().as_bytes());
    home.join("cache").join(NAME).join(format!("{name}-{}", &hash[..16]))
}

/// The `java` arguments that run jdtls (those of jdtls's own `bin/jdtls`).
pub fn jdtls_args(jdtls: &Jdtls, data: &Path) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    vec![
        "-Declipse.application=org.eclipse.jdt.ls.core.id1".into(),
        "-Dosgi.bundles.defaultStartLevel=4".into(),
        "-Declipse.product=org.eclipse.jdt.ls.core.product".into(),
        "-Dosgi.checkConfiguration=true".into(),
        format!("-Dosgi.sharedConfiguration.area={}", s(&jdtls.configuration)),
        "-Dosgi.sharedConfiguration.area.readOnly=true".into(),
        "-Dosgi.configuration.cascaded=true".into(),
        "-Xms1G".into(),
        "--add-modules=ALL-SYSTEM".into(),
        "--add-opens".into(),
        "java.base/java.util=ALL-UNNAMED".into(),
        "--add-opens".into(),
        "java.base/java.lang=ALL-UNNAMED".into(),
        "-jar".into(),
        s(&jdtls.launcher),
        "-data".into(),
        s(data),
    ]
}

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.language_ids = vec![("java".into(), "java".into())];
    // Importing a Gradle or Maven project (dependency resolution, the first full build) is slow.
    spec.init_timeout = Duration::from_secs(180);
    spec.diagnostics_wait = Duration::from_secs(180);
    spec.query_wait = Duration::from_secs(120);
    spec
}

fn install_hint(toolchains: &Toolchains) -> String {
    format!("`lang-java.install` installs jdtls {JDTLS_VERSION} (and Temurin {TEMURIN_VERSION} when there is no JDK {MIN_JAVA}+) into {}", toolchains.root().display())
}

/// jdtls is runnable: installed, with a JDK 21+. The executable is that JDK's `java`.
pub fn locate(toolchains: &Toolchains, host: &Host) -> Result<FoundServer, String> {
    let jdtls = installed_jdtls(toolchains).map_err(|problem| format!("{NAME} is not installed: {problem}; {}", install_hint(toolchains)))?;
    let jdk = jdk_for_jdtls(toolchains, host).map_err(|problem| format!("{NAME} {JDTLS_VERSION} needs Java {MIN_JAVA}+: {problem}; {}", install_hint(toolchains)))?;
    Ok(FoundServer { path: jdk.java(), version: format!("{NAME} {JDTLS_VERSION} on Java {} ({})", jdk.version, jdtls.dir.display()) })
}

/// How jdtls starts for `root`: the java arguments with the root's data directory (created here),
/// or, for a configured server executable (`configured`: a `jdtls` script), only `-data <dir>`.
pub fn launch(home: &Path, toolchains: &Toolchains, root: &Path, configured: bool) -> Result<Launch, String> {
    let data = data_dir(home, root);
    std::fs::create_dir_all(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
    let args = if configured { vec!["-data".to_owned(), data.to_string_lossy().into_owned()] } else { jdtls_args(&installed_jdtls(toolchains)?, &data) };
    Ok(Launch { args: Some(args), ..Launch::default() })
}

/// Install the JDK (when none 21+ is present) and jdtls.
pub fn install(toolchains: &Toolchains, host: &Host, log: &mut Vec<String>) -> Result<(), String> {
    match (installed_jdk(toolchains), system_jdk(host)) {
        (Ok(jdk), _) => log.push(format!("Temurin {} is already installed at {}", jdk.version, jdk.home.display())),
        (Err(_), Ok(jdk)) if jdk.major >= MIN_JAVA => log.push(format!("using the system's JDK {} at {} (no JDK installed)", jdk.version, jdk.home.display())),
        (Err(_), system) => {
            match system {
                Ok(jdk) => log.push(format!("the system's JDK is Java {} ({}); installing Temurin {TEMURIN_VERSION}", jdk.version, jdk.home.display())),
                Err(problem) => log.push(format!("{problem}; installing Temurin {TEMURIN_VERSION}")),
            }
            toolchains.download_release("jdk", &temurin_release()?, log)?;
            let jdk = installed_jdk(toolchains).map_err(|e| format!("the installed JDK does not run: {e}"))?;
            log.push(format!("Temurin {} at {}", jdk.version, jdk.home.display()));
        }
    }
    toolchains.download_release(NAME, &jdtls_release(), log)?;
    let jdtls = installed_jdtls(toolchains)?;
    log.push(format!("jdtls launcher {}", jdtls.launcher.display()));
    Ok(())
}

/// What commands need: the installed JDK when the system has none.
pub fn toolchain(toolchains: &Toolchains, host: &Host) -> Toolchain {
    match installed_jdk(toolchains) {
        Ok(jdk) if system_jdk(host).is_err() => {
            Toolchain { path: vec![jdk.home.join("bin").to_string_lossy().into_owned()], env: vec![("JAVA_HOME".into(), jdk.home.to_string_lossy().into_owned())] }
        }
        _ => Toolchain::default(),
    }
}
