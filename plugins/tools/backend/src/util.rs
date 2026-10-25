//! Small shared helpers: ids, content hashes, path resolution and atomic file writes.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// A short random id with a prefix, e.g. `a-3f9c0d1e2b4a`.
pub fn new_id(prefix: &str) -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &raw[..12])
}

/// Hex SHA-256 of some bytes (used as the identity of a file version).
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

/// Expand a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if path == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(path)
}

/// Lexically normalize a path (resolve `.` and `..` without touching the file system).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path given by the model or the UI against a base directory.
/// The workspace's scratch folder, `tmp/` at its root: its shells' `$TMPDIR`, where throwaway
/// checks live (short relative paths, removed with the workspace). Git ignores it.
/// Whether a path in a project is a test file by the usual layouts: inside a `test`, `tests`,
/// `__tests__`, `spec`, `specs` or `testing` folder, or named like one (`test_x.py`, `x_test.py`,
/// `x_test.go`, `x.test.ts`, `x.spec.js`, `XTest.java`, `XTests.cs`, `x_spec.rb`, `test.x`).
pub fn is_test_path(relative: &Path) -> bool {
    let folder = relative.parent().is_some_and(|parent| {
        parent.components().any(|part| matches!(part.as_os_str().to_str(), Some("test" | "tests" | "__tests__" | "spec" | "specs" | "testing")))
    });
    let name = relative.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let stem = name.split('.').next().unwrap_or("");
    folder
        || stem == "test"
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || name.contains(".test.")
        || name.contains(".spec.")
}

pub fn scratch_dir(workspace: &Path) -> PathBuf {
    workspace.join("tmp")
}

/// The scratch folder, made and listed in the workspace's `.gitignore` (once: a line that already
/// ignores `tmp` stays as it is).
pub fn ensure_scratch(workspace: &Path) -> PathBuf {
    let scratch = scratch_dir(workspace);
    let _ = std::fs::create_dir_all(&scratch);
    if workspace.join(".git").exists() {
        let ignore = workspace.join(".gitignore");
        let text = std::fs::read_to_string(&ignore).unwrap_or_default();
        if !text.lines().any(|line| matches!(line.trim(), "tmp" | "tmp/" | "/tmp" | "/tmp/")) {
            let separator = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
            let _ = std::fs::write(&ignore, format!("{text}{separator}/tmp/\n"));
        }
    }
    scratch
}

/// The project's own environment for its commands: `KEY=VALUE` lines of the nearest `.cortex-env`
/// in `dir` or a folder above it (a toolchain the machine does not have on its PATH, e.g.
/// `JAVA_HOME=/opt/jdk` and `PATH=$JAVA_HOME/bin:$PATH`). `#` starts a comment; `$NAME` and
/// `${NAME}` take an earlier line's value or the process's. Empty when there is no such file.
pub fn project_env(dir: &Path) -> Vec<(String, String)> {
    let Some(file) = dir.ancestors().take(8).map(|folder| folder.join(".cortex-env")).find(|file| file.is_file()) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&file) else { return Vec::new() };
    let mut vars: Vec<(String, String)> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim().trim_matches('"');
        let lookup = |name: &str| {
            vars.iter().rev().find(|(known, _)| known == name).map(|(_, value)| value.clone()).or_else(|| std::env::var(name).ok()).unwrap_or_default()
        };
        let mut expanded = String::new();
        let mut rest = value;
        while let Some(at) = rest.find('$') {
            expanded.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let (name, tail) = if let Some(inner) = after.strip_prefix('{') {
                match inner.find('}') {
                    Some(end) => (&inner[..end], &inner[end + 1..]),
                    None => ("", after),
                }
            } else {
                let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
                (&after[..end], &after[end..])
            };
            if name.is_empty() {
                expanded.push('$');
                rest = after;
            } else {
                expanded.push_str(&lookup(name));
                rest = tail;
            }
        }
        expanded.push_str(rest);
        vars.push((key.to_owned(), expanded));
    }
    vars
}

/// `path` with a leading `$TMPDIR` or `${TMPDIR}` replaced by the session's scratch folder.
pub fn expand_scratch(path: &str, scratch: &Path) -> String {
    let path = path.trim();
    for prefix in ["${TMPDIR}", "$TMPDIR"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return format!("{}{rest}", scratch.display());
        }
    }
    path.to_owned()
}

pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let expanded = expand_home(path.trim());
    let joined = if expanded.is_absolute() { expanded } else { base.join(expanded) };
    normalize(&joined)
}

/// Canonical form for containment checks: canonicalize the longest existing ancestor and
/// re-append the rest (so not-yet-existing files still resolve through symlinked parents).
pub fn canonical_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name().map(|name| name.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return normalize(path),
        }
    }
}

/// Whether `path` lies inside `root` (both resolved through symlinks where they exist).
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let path = canonical_lenient(path);
    let root = canonical_lenient(root);
    path.starts_with(&root)
}

/// Display a path relative to `base` when it is inside it.
pub fn display_rel(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        Ok(_) => ".".to_owned(),
        Err(_) => {
            let canonical_base = canonical_lenient(base);
            let canonical_path = canonical_lenient(path);
            match canonical_path.strip_prefix(&canonical_base) {
                Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
                _ => path.to_string_lossy().into_owned(),
            }
        }
    }
}

/// Resolve the opened object, rather than a path a command can replace after validation.
fn descriptor_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this live descriptor's buffer.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let end = buffer.iter().position(|byte| *byte == 0).ok_or_else(|| std::io::Error::other("opened path is not terminated"))?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "host file access needs macOS descriptor path verification"))
    }
}

pub fn open_host_read(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::File::open(path)?;
    let actual = descriptor_path(&file)?;
    if crate::sandbox::host_control_read_denied(&actual) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "app control files are private; use cx for command access"));
    }
    Ok(file)
}

pub fn read_host_file(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let mut file = open_host_read(path.as_ref())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_host_text(path: impl AsRef<Path>) -> std::io::Result<String> {
    let mut file = open_host_read(path.as_ref())?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// Open a parent directory by descriptor, checking every newly created path before mutating it.
fn checked_parent(path: &Path, create: bool, allowed: &impl Fn(&Path) -> bool) -> std::io::Result<(std::fs::File, std::ffi::CString)> {
    let target = normalize(&if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) });
    let parent = target.parent().ok_or_else(|| std::io::Error::other("file has no parent"))?;
    let name = target.file_name().ok_or_else(|| std::io::Error::other("file has no name"))?;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("file name contains NUL"))?;
    let mut directory = std::fs::File::open("/")?;
    for component in parent.components() {
        let Component::Normal(component) = component else { continue };
        let next_path = descriptor_path(&directory)?.join(component);
        let component = std::ffi::CString::new(component.as_bytes()).map_err(|_| std::io::Error::other("directory name contains NUL"))?;
        // SAFETY: directory and CString remain alive throughout each descriptor-relative call.
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), component.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if create && fd == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if !allowed(&next_path) { return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "app control paths are managed by the app")); }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o755) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists { return Err(error); }
            }
            fd = unsafe { libc::openat(directory.as_raw_fd(), component.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        }
        if fd == -1 { return Err(std::io::Error::last_os_error()); }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let actual = descriptor_path(&directory)?.join(std::ffi::OsStr::from_bytes(name.as_bytes()));
    if !allowed(&actual) { return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "app control paths are managed by the app")); }
    Ok((directory, name))
}

/// Native model/editor writes cannot cross into app control paths through a replaced symlink.
pub fn atomic_write_checked(path: &Path, bytes: &[u8], allowed: impl Fn(&Path) -> bool) -> std::io::Result<()> {
    let target = canonical_lenient(path);
    let permissions = std::fs::metadata(&target).ok().map(|metadata| metadata.permissions());
    let (directory, name) = checked_parent(&target, true, &allowed)?;
    let temporary = std::ffi::CString::new(format!(".cortex-{}", uuid::Uuid::new_v4().simple())).expect("ASCII temporary name");
    let fd = unsafe { libc::openat(directory.as_raw_fd(), temporary.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW, 0o666) };
    if fd == -1 { return Err(std::io::Error::last_os_error()); }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(bytes)?;
        if let Some(permissions) = permissions { file.set_permissions(permissions)?; }
        file.sync_all()?;
        let actual = descriptor_path(&directory)?.join(std::ffi::OsStr::from_bytes(name.as_bytes()));
        if !allowed(&actual) { return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "app control paths are managed by the app")); }
        if unsafe { libc::renameat(directory.as_raw_fd(), temporary.as_ptr(), directory.as_raw_fd(), name.as_ptr()) } == -1 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    })();
    if result.is_err() { unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0); } }
    result
}

pub fn remove_file_checked(path: &Path, allowed: impl Fn(&Path) -> bool) -> std::io::Result<()> {
    let (directory, name) = checked_parent(path, false, &allowed)?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == -1 { return Err(std::io::Error::last_os_error()); }
    Ok(())
}

/// Write a file atomically (temp file in the same directory, then rename), keeping the
/// permissions of an existing file. Symlinks are followed so the link target is updated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.cortex-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temp, permissions)?;
        }
        std::fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Quote a string for POSIX shells (single quotes).
pub fn shell_quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b)) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Command output with the workspace's own absolute paths written relative to it (`<ws>/tests/a.py`
/// becomes `tests/a.py`; macOS names temp folders both as `/var` and `/private/var`, so both
/// spellings). A tool or traceback printing the long absolute path gave the model folder names to
/// `cd` into and to repeat in file paths (seen: a copy of a file written at `<ws tail>/file.py`).
pub fn relative_paths(text: &str, workspace: &Path) -> String {
    let base = workspace.to_string_lossy().trim_end_matches('/').to_owned();
    if base.is_empty() || base == "/" {
        return text.to_owned();
    }
    let plain = base.strip_prefix("/private").unwrap_or(&base).to_owned();
    let mut out = text.to_owned();
    for form in [format!("/private{plain}/"), format!("{plain}/")] {
        out = out.replace(&form, "");
    }
    out
}
