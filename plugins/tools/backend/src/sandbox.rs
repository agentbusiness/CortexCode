//! The write sandbox every agent command runs in (macOS `sandbox-exec`).
//!
//! The profile is structural: it names directories, never commands. Reads, process execution and
//! the network stay as they are; a write (create, modify, delete, rename, chmod, xattr, times) is
//! allowed only under:
//! - the session workspace (and, when the workspace is a linked git worktree, its git directories);
//! - CORTEX_HOME and this plugin's data directory;
//! - temp dirs: /private/tmp, /private/var/tmp, /private/var/folders (holds $TMPDIR and the per-user
//!   cache dir), and $TMPDIR itself wherever it points;
//! - package caches: cargo's registry, git checkouts and lock/cache files, ~/.npm, the uv cache, the
//!   pip cache, node-gyp's, yarn's, pnpm's, bun's and Go's module/build caches;
//! - the session's Python venv;
//! - directories the user approved for the session (`write_outside:<dir>` approvals) and any listed
//!   in the plugin's `sandbox_write_paths` config;
//! - /dev (terminals, /dev/null, /dev/fd; device nodes are guarded by their own permissions).
//!
//! A workspace holding a `.cortex-offline` file is offline: its commands get no outbound network
//! but localhost and unix sockets (a benchmark workspace, or a project its owner keeps offline).
//!
//! Inside CORTEX_HOME a few paths stay read-only for commands because writing them would grant
//! authority outside the sandbox: the approvals file, the frame's plugin store (executables the frame
//! runs unsandboxed), its run directory (token, socket, lock), the session journals, and background
//! job metadata (the plugin signals the process group named there).
//!
//! The sandbox reports nothing to the parent process, so a refused write shows up as the OS error
//! EPERM ("Operation not permitted") in the command's own output; [`mentions_eperm`] recognizes that
//! fixed `strerror(EPERM)` text (a mechanical rule, not a judgement) so the model can be told why.

use crate::util;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Approval classes that widen the sandbox for a session: `write_outside:<absolute dir>`.
pub const WRITE_CLASS_PREFIX: &str = cc_protocol::contracts::tools::approval_classes::WRITE_OUTSIDE_PREFIX;

/// How one shell or job is confined: `sandbox-exec [-D key=value]... -p <profile> <program>`.
/// The file whose presence makes a workspace offline (see the module doc).
pub const OFFLINE_MARKER: &str = ".cortex-offline";

/// Folders an offline workspace's commands and reads may not see: the marker's lines that are
/// absolute paths (a benchmark lists its hidden tests and reference solutions there).
pub fn denied_reads(workspace: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(workspace.join(OFFLINE_MARKER))
        .map(|text| text.lines().map(str::trim).filter(|line| line.starts_with('/')).map(|line| real(Path::new(line))).collect())
        .unwrap_or_default()
}

/// Whether `path` lies under a folder the workspace's marker denies.
pub fn read_denied(workspace: &Path, path: &Path) -> bool {
    let denied = denied_reads(workspace);
    !denied.is_empty() && {
        let path = real(path);
        denied.iter().any(|folder| path.starts_with(folder))
    }
}

/// The rules of an offline workspace (none otherwise): no outbound network but localhost (the
/// frame, language servers, local services) and unix sockets, and no reads of the folders its
/// marker lists.
pub fn network_rules(workspace: &Path) -> String {
    if !workspace.join(OFFLINE_MARKER).exists() {
        return String::new();
    }
    let mut rules = "(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\"))\n(allow network-outbound (remote unix-socket))\n".to_owned();
    for folder in denied_reads(workspace) {
        if let Some(text) = sbpl_string(&folder) {
            rules.push_str(&format!("(deny file-read* (subpath {text}))\n"));
        }
    }
    rules
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub exec: PathBuf,
    pub profile: String,
    pub params: Vec<(String, String)>,
}

impl Sandbox {
    /// Arguments for `sandbox-exec` before the confined program.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (key, value) in &self.params {
            args.push("-D".to_owned());
            args.push(format!("{key}={value}"));
        }
        args.push("-p".to_owned());
        args.push(self.profile.clone());
        args
    }

    /// Identity of the confinement (a shell started under another one must be restarted).
    pub fn key(&self) -> String {
        let mut text = self.profile.clone();
        for (key, value) in &self.params {
            text.push_str(&format!("\n-D {key}={value}"));
        }
        util::content_hash(text.as_bytes())
    }
}

/// The machine-wide part of the policy, computed once per plugin instance.
#[derive(Debug, Clone)]
pub struct Policy {
    /// `sandbox-exec`, when this system has it.
    pub exec: Option<PathBuf>,
    /// Directories writable for every session (home, data dir, temp, caches, configured extras).
    pub roots: Vec<PathBuf>,
    /// Single files writable for every session (cargo's lock and cache databases).
    pub files: Vec<PathBuf>,
    /// Paths that stay read-only even inside a writable root.
    pub protected_subpaths: Vec<PathBuf>,
    pub protected_literals: Vec<PathBuf>,
    /// Regex rules (already escaped) for protected files with variable names.
    pub protected_regexes: Vec<String>,
}

/// Real path of `path` (through symlinks of its existing ancestors): the sandbox matches the
/// resolved path, so every rule names the resolved form.
fn real(path: &Path) -> PathBuf {
    util::canonical_lenient(&util::normalize(path))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from).filter(|path| path.is_absolute())
}

fn user_home() -> Option<PathBuf> {
    env_path("HOME")
}

/// Package-cache locations of the common toolchains, honouring each tool's documented override
/// variable as the plugin's environment sets it (the model's shell cannot move them).
fn cache_roots(home: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut roots = Vec::new();
    let mut files = Vec::new();
    let cargo = env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    roots.push(cargo.join("registry"));
    roots.push(cargo.join("git"));
    for name in [".package-cache", ".package-cache-mutate", ".global-cache"] {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            files.push(cargo.join(format!("{name}{suffix}")));
        }
    }
    roots.push(env_path("npm_config_cache").or_else(|| env_path("NPM_CONFIG_CACHE")).unwrap_or_else(|| home.join(".npm")));
    let xdg_cache = env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
    roots.push(env_path("UV_CACHE_DIR").unwrap_or_else(|| xdg_cache.join("uv")));
    roots.push(env_path("PIP_CACHE_DIR").unwrap_or_else(|| home.join("Library/Caches/pip")));
    let caches = home.join("Library/Caches");
    for name in ["node-gyp", "Yarn", "pnpm", "go-build", "uv"] {
        roots.push(caches.join(name));
    }
    roots.push(home.join("Library/pnpm/store"));
    roots.push(home.join(".bun/install/cache"));
    if let Some(gocache) = env_path("GOCACHE") {
        roots.push(gocache);
    }
    let gopath = env_path("GOPATH").unwrap_or_else(|| home.join("go"));
    roots.push(env_path("GOMODCACHE").unwrap_or_else(|| gopath.join("pkg/mod")));
    (roots, files)
}

fn temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/private/tmp"), PathBuf::from("/private/var/tmp"), PathBuf::from("/private/var/folders")];
    roots.push(std::env::temp_dir());
    if let Some(tmp) = env_path("TMPDIR") {
        roots.push(tmp);
    }
    roots
}

/// Escape a path for an SBPL regex literal (structural: every regex metacharacter gets a backslash).
fn regex_escape(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.contains('"') {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    Some(out)
}

/// An SBPL string literal, or None for paths the profile language cannot carry safely.
fn sbpl_string(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths.into_iter().filter(|path| seen.insert(path.clone())).collect()
}

/// Drop roots inside other roots (a project venv inside the workspace, the automatic venv inside
/// CORTEX_HOME): the profile, and so the shell, stays the same when only the venv changes.
fn outermost(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let paths = dedup(paths);
    paths.iter().filter(|path| !paths.iter().any(|other| other != *path && path.starts_with(other))).cloned().collect()
}

impl Policy {
    pub fn new(cortex_home: &Path, data_dir: &Path, extra: &[String]) -> Policy {
        let exec = Path::new(SANDBOX_EXEC).is_file().then(|| PathBuf::from(SANDBOX_EXEC));
        let mut roots = vec![cortex_home.to_path_buf(), data_dir.to_path_buf()];
        roots.extend(temp_roots());
        let mut files = Vec::new();
        if let Some(home) = user_home() {
            let (cache_dirs, cache_files) = cache_roots(&home);
            roots.extend(cache_dirs);
            files.extend(cache_files);
        }
        for entry in extra {
            let path = util::expand_home(entry.trim());
            if path.is_absolute() {
                roots.push(path);
            }
        }
        let roots = dedup(roots.iter().map(|path| real(path)).collect());
        let files = dedup(files.iter().map(|path| real(path)).collect());
        let home = real(cortex_home);
        let data = real(data_dir);
        let protected_subpaths = vec![home.join("plugins"), home.join("run"), home.join("journal")];
        let mut protected_literals =
            vec![data.join("approvals.json"), home.clone(), home.join("plugins-data"), data.clone(), home.join("jobs")];
        protected_literals.extend(protected_subpaths.iter().cloned());
        let protected_regexes = regex_escape(&home.join("jobs"))
            .map(|jobs| vec![format!("^{jobs}/[^/]+/(meta\\.json|cmd\\.sh|runner\\.sh)$")])
            .unwrap_or_default();
        Policy { exec, roots, files, protected_subpaths, protected_literals: dedup(protected_literals), protected_regexes }
    }

    pub fn available(&self) -> bool {
        self.exec.is_some()
    }

    /// Writable roots of one session: the base roots plus its workspace, git dirs, venv and grants.
    pub fn session_roots(&self, workspace: &Path, venv: Option<&Path>, grants: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots = vec![real(workspace)];
        roots.extend(linked_git_dirs(workspace));
        if let Some(venv) = venv {
            roots.push(real(venv));
        }
        roots.extend(grants.iter().map(|path| real(path)));
        roots.extend(self.roots.iter().cloned());
        outermost(roots)
    }

    /// Whether `path` is one of the paths that stay read-only for commands whatever is approved.
    pub fn protected(&self, path: &Path) -> bool {
        let path = real(path);
        self.protected_subpaths.iter().any(|protected| path.starts_with(protected))
            || self.protected_literals.contains(&path)
            || self
                .protected_regexes
                .iter()
                .any(|pattern| regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(&path.to_string_lossy())))
    }

    /// Whether the sandbox of this session lets commands write `path` (protected paths excluded).
    pub fn writable(&self, path: &Path, workspace: &Path, venv: Option<&Path>, grants: &[PathBuf]) -> bool {
        if self.protected(path) {
            return false;
        }
        let path = real(path);
        self.files.contains(&path) || self.session_roots(workspace, venv, grants).iter().any(|root| path.starts_with(root))
    }

    /// The deny rules of the profile (without the `(version 1)(allow default)` header).
    pub fn rules(&self, workspace: &Path, venv: Option<&Path>, grants: &[PathBuf]) -> String {
        let mut allowed = String::from("      (subpath \"/dev\")\n");
        for root in self.session_roots(workspace, venv, grants) {
            if let Some(text) = sbpl_string(&root) {
                allowed.push_str(&format!("      (subpath {text})\n"));
            }
        }
        for file in &self.files {
            if let Some(text) = sbpl_string(file) {
                allowed.push_str(&format!("      (literal {text})\n"));
            }
        }
        let mut protected = String::new();
        for path in &self.protected_subpaths {
            if let Some(text) = sbpl_string(path) {
                protected.push_str(&format!("  (subpath {text})\n"));
            }
        }
        for path in &self.protected_literals {
            if let Some(text) = sbpl_string(path) {
                protected.push_str(&format!("  (literal {text})\n"));
            }
        }
        for regex in &self.protected_regexes {
            protected.push_str(&format!("  (regex #\"{regex}\")\n"));
        }
        format!("(deny file-write*\n  (require-not\n    (require-any\n{allowed}    )))\n(deny file-write*\n{protected})\n")
    }

    /// The complete profile for a session's shell or job.
    pub fn sandbox(&self, workspace: &Path, venv: Option<&Path>, grants: &[PathBuf]) -> Option<Sandbox> {
        let exec = self.exec.clone()?;
        let profile = format!("(version 1)\n(allow default)\n{}{}", network_rules(workspace), self.rules(workspace, venv, grants));
        Some(Sandbox { exec, profile, params: Vec::new() })
    }

    /// A caller-supplied profile (from a `sandbox-exec -p ...` wrapper) intersected with the session's
    /// write rules: their rules first, ours last (in SBPL the last matching rule wins).
    pub fn compose(&self, inner: &InnerProfile, workspace: &Path, venv: Option<&Path>, grants: &[PathBuf]) -> Option<Sandbox> {
        let exec = self.exec.clone()?;
        let mut profile = String::new();
        if !inner.has_version {
            profile.push_str("(version 1)\n");
        }
        for form in &inner.forms {
            profile.push_str(form);
            profile.push('\n');
        }
        profile.push_str(&network_rules(workspace));
        profile.push_str(&self.rules(workspace, venv, grants));
        Some(Sandbox { exec, profile, params: inner.params.clone() })
    }
}

/// Git directories of a linked worktree workspace (`.git` is a file `gitdir: <dir>`): git writes
/// the worktree's HEAD and index there and objects/refs in the common dir.
fn linked_git_dirs(workspace: &Path) -> Vec<PathBuf> {
    let dot_git = workspace.join(".git");
    if !dot_git.is_file() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&dot_git) else { return Vec::new() };
    let Some(gitdir) = text.lines().find_map(|line| line.strip_prefix("gitdir:")).map(str::trim) else { return Vec::new() };
    let gitdir = util::resolve(workspace, gitdir);
    let mut dirs = vec![real(&gitdir)];
    if let Ok(common) = std::fs::read_to_string(gitdir.join("commondir")) {
        dirs.push(real(&util::resolve(&gitdir, common.trim())));
    }
    dirs
}

/// Paths granted by `write_outside:<dir>` approvals.
pub fn grants(approved: &BTreeSet<String>) -> Vec<PathBuf> {
    approved
        .iter()
        .filter_map(|class| class.strip_prefix(WRITE_CLASS_PREFIX))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect()
}

/// The approval class for writing under `path`.
pub fn write_class(path: &Path) -> String {
    format!("{WRITE_CLASS_PREFIX}{}", real(path).display())
}

/// Whether command output contains the text of `strerror(EPERM)`, which is what a refused write
/// looks like from inside the sandbox (mechanical match of the OS error message, any case).
pub fn mentions_eperm(bytes: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"operation not permitted";
    bytes.windows(NEEDLE.len()).any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

/// The absolute paths (`/x`, `~/x`) on the lines of command output that report EPERM: what a write
/// the sandbox refused names. Structural: the OS error text, then the path-shaped words of that
/// line (split at whitespace, quotes, `:` and `,`).
pub fn eperm_paths(bytes: &[u8]) -> Vec<PathBuf> {
    const NEEDLE: &str = "operation not permitted";
    let text = String::from_utf8_lossy(bytes);
    let mut paths = Vec::new();
    for line in text.lines().filter(|line| line.to_ascii_lowercase().contains(NEEDLE)) {
        for word in line.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | ':' | ',' | '(' | ')')) {
            let path = if let Some(rest) = word.strip_prefix("~/") {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
            } else {
                word.starts_with('/').then(|| PathBuf::from(word))
            };
            if let Some(path) = path
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
    }
    paths
}

// ---------------------------------------------------------------------------------------------
// Caller-supplied profiles (`sandbox-exec -p <profile> <command...>` run through `tools.run`).
// A sandboxed process cannot apply a second sandbox, so such a wrapper is unwrapped and its profile
// intersected with the session's rules. The profile is accepted only when it is plain rules: a
// tokenizer that mirrors SBPL's lexical syntax splits it into top-level forms; only `version`,
// `allow`, `deny` and `debug` forms are allowed, and no form may bind or evaluate anything (which
// could redefine the rule constructors used after it).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerProfile {
    pub forms: Vec<String>,
    pub has_version: bool,
    pub params: Vec<(String, String)>,
}

/// A `sandbox-exec` invocation split into its profile and the confined command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    pub profile: InnerProfile,
    pub argv: Vec<String>,
}

const FORBIDDEN_ATOMS: &[&str] = &[
    "define",
    "define-syntax",
    "define-macro",
    "defmacro",
    "macro",
    "set!",
    "lambda",
    "let",
    "let*",
    "letrec",
    "letrec*",
    "named-lambda",
    "begin",
    "eval",
    "import",
    "load",
    "apply",
    "call/cc",
    "call-with-current-continuation",
];

/// Split an SBPL profile into validated top-level forms.
pub fn parse_profile(text: &str) -> Result<(Vec<String>, bool), String> {
    let bytes = text.as_bytes();
    let mut forms = Vec::new();
    let mut has_version = false;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut head_expected = false;
    let mut top_head: Option<String> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                if depth == 0 {
                    start = i;
                    top_head = None;
                }
                depth += 1;
                head_expected = true;
                i += 1;
            }
            b')' => {
                if depth == 0 {
                    return Err("unbalanced `)`".to_owned());
                }
                depth -= 1;
                head_expected = false;
                i += 1;
                if depth == 0 {
                    let head = top_head.take().unwrap_or_default();
                    match head.as_str() {
                        "version" => has_version = true,
                        "allow" | "deny" | "debug" => {}
                        other => return Err(format!("top-level form `{other}` is not a rule")),
                    }
                    forms.push(text[start..i].to_owned());
                }
            }
            b'"' => {
                if depth == 0 {
                    return Err("a string outside any form".to_owned());
                }
                i += 1;
                loop {
                    match bytes.get(i) {
                        None => return Err("unterminated string".to_owned()),
                        Some(b'\\') => i += 2,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                head_expected = false;
            }
            b'#' => {
                if depth == 0 {
                    return Err("`#` outside any form".to_owned());
                }
                match bytes.get(i + 1) {
                    Some(b'"') => {
                        i += 2;
                        loop {
                            match bytes.get(i) {
                                None => return Err("unterminated regex".to_owned()),
                                Some(b'\\') if bytes.get(i + 1) == Some(&b'"') => {
                                    return Err("escaped quote inside a regex literal".to_owned());
                                }
                                Some(b'"') => {
                                    i += 1;
                                    break;
                                }
                                Some(_) => i += 1,
                            }
                        }
                    }
                    Some(b't') | Some(b'f') if !bytes.get(i + 2).is_some_and(|b| is_atom_byte(*b)) => i += 2,
                    _ => return Err("unsupported `#` syntax".to_owned()),
                }
                head_expected = false;
            }
            b'\'' | b'`' | b',' | b'|' | b'[' | b']' | b'{' | b'}' => return Err(format!("unsupported character `{}`", c as char)),
            _ => {
                if depth == 0 {
                    return Err("an atom outside any form".to_owned());
                }
                let begin = i;
                while i < bytes.len() && is_atom_byte(bytes[i]) {
                    i += 1;
                }
                if i == begin {
                    return Err(format!("unexpected byte {c:#x}"));
                }
                let atom = &text[begin..i];
                if FORBIDDEN_ATOMS.contains(&atom) {
                    return Err(format!("`{atom}` is not allowed in a command's sandbox profile"));
                }
                if head_expected && depth == 1 {
                    top_head = Some(atom.to_owned());
                }
                head_expected = false;
            }
        }
    }
    if depth != 0 {
        return Err("unbalanced `(`".to_owned());
    }
    Ok((forms, has_version))
}

fn is_atom_byte(b: u8) -> bool {
    !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' | b'"' | b';' | b'\'' | b'`' | b',' | b'#' | b'|' | b'[' | b']' | b'{' | b'}')
}

/// Recognize `sandbox-exec [-f file | -p profile] [-D key=value]... command...` (argv already split
/// into literal words). A relative `-f` path resolves against `cwd`.
pub fn unwrap_sandbox_exec(argv: &[String], cwd: &Path) -> Option<Result<Wrapped, String>> {
    let program = argv.first()?;
    if program.rsplit('/').next() != Some("sandbox-exec") {
        return None;
    }
    let mut profile_text: Option<String> = None;
    let mut params = Vec::new();
    let mut index = 1;
    let take_value = |index: &mut usize, flag: &str, attached: &str| -> Result<String, String> {
        if !attached.is_empty() {
            return Ok(attached.to_owned());
        }
        *index += 1;
        argv.get(*index).cloned().ok_or_else(|| format!("sandbox-exec {flag} needs a value"))
    };
    while index < argv.len() {
        let word = &argv[index];
        if word == "--" {
            index += 1;
            break;
        }
        if !word.starts_with('-') || word.len() < 2 {
            break;
        }
        let (flag, attached) = word.split_at(2);
        let result = match flag {
            "-p" => take_value(&mut index, flag, attached).map(|value| profile_text = Some(value)),
            "-f" => take_value(&mut index, flag, attached).and_then(|value| {
                let path = util::resolve(cwd, &value);
                std::fs::read_to_string(&path)
                    .map(|text| profile_text = Some(text))
                    .map_err(|error| format!("sandbox-exec -f {}: {error}", path.display()))
            }),
            "-D" => take_value(&mut index, flag, attached).and_then(|value| match value.split_once('=') {
                Some((key, val)) if !key.is_empty() => {
                    params.push((key.to_owned(), val.to_owned()));
                    Ok(())
                }
                _ => Err(format!("sandbox-exec -D {value}: expected key=value")),
            }),
            other => Err(format!("sandbox-exec option {other} is not supported inside the write sandbox")),
        };
        if let Err(error) = result {
            return Some(Err(error));
        }
        index += 1;
    }
    let Some(text) = profile_text else { return Some(Err("sandbox-exec without -p or -f".to_owned())) };
    let rest: Vec<String> = argv[index..].to_vec();
    if rest.is_empty() {
        return Some(Err("sandbox-exec without a command".to_owned()));
    }
    Some(parse_profile(&text).map(|(forms, has_version)| Wrapped { profile: InnerProfile { forms, has_version, params }, argv: rest }))
}
