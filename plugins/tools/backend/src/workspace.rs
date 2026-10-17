//! Copy-on-write workspaces: an agent's private clone of the project made with APFS `clonefile(2)`
//! (the whole directory tree in one syscall, sharing data blocks until written), including untracked
//! and ignored files, with its own git branch. Falls back to a plain recursive copy only when the
//! volume cannot clone, and reports which method was used.

use crate::util;
use cc_protocol::contracts::tools::Workspace;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Workspaces {
    root: PathBuf,
}

#[derive(Debug)]
pub struct WorkspaceError {
    pub code: &'static str,
    pub message: String,
}

impl WorkspaceError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
}

/// clonefile(2): returns Ok(true) when cloned, Ok(false) when the volume does not support it.
fn clone_tree(source: &Path, target: &Path) -> std::io::Result<bool> {
    let src = CString::new(source.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let dst = CString::new(target.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: both arguments are valid NUL-terminated paths; flags 0 = follow nothing special.
    let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => Ok(false),
        _ => Err(error),
    }
}

fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        std::os::unix::fs::symlink(link, target)?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(target)?;
        std::fs::set_permissions(target, meta.permissions())?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(source, target)?;
    }
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

impl Workspaces {
    pub fn new(home: &Path) -> Self {
        Self { root: home.join("workspaces") }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn meta_path(path: &Path) -> PathBuf {
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        path.with_file_name(format!(".{name}.workspace.json"))
    }

    pub fn create(&self, project_id: &str, source: &str, name: &str, branch: Option<&str>) -> Result<Workspace, WorkspaceError> {
        if !valid_segment(project_id) {
            return Err(WorkspaceError::new("invalid_params", format!("project id `{project_id}` is not a valid directory name")));
        }
        if !valid_segment(name) {
            return Err(WorkspaceError::new(
                "invalid_params",
                format!("workspace name `{name}` must be letters, digits, `-`, `_`, `.` or `@`"),
            ));
        }
        let source = util::expand_home(source);
        let source =
            source.canonicalize().map_err(|error| WorkspaceError::new("not_found", format!("source {}: {error}", source.display())))?;
        if !source.is_dir() {
            return Err(WorkspaceError::new("invalid_params", format!("source {} is not a directory", source.display())));
        }
        let parent = self.root.join(project_id);
        std::fs::create_dir_all(&parent).map_err(|error| WorkspaceError::new("internal", format!("{}: {error}", parent.display())))?;
        let target = parent.join(name);
        if target.exists() {
            return Err(WorkspaceError::new("conflict", format!("workspace {} already exists", target.display())));
        }
        if util::is_inside(&target, &source) {
            return Err(WorkspaceError::new("invalid_params", "the workspace directory cannot be inside the project it clones"));
        }
        let method = match clone_tree(&source, &target) {
            Ok(true) => "clonefile",
            Ok(false) => {
                copy_tree(&source, &target).map_err(|error| {
                    let _ = std::fs::remove_dir_all(&target);
                    WorkspaceError::new("internal", format!("copy failed: {error}"))
                })?;
                "copy"
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&target);
                return Err(WorkspaceError::new("internal", format!("clonefile {} -> {}: {error}", source.display(), target.display())));
            }
        };
        let mut branch_name = String::new();
        if target.join(".git").exists() {
            let wanted = branch.map(str::to_owned).unwrap_or_else(|| format!("cortex/{name}"));
            if let Err(error) = git(&target, &["switch", "-c", &wanted]).or_else(|_| git(&target, &["checkout", "-b", &wanted])) {
                let _ = std::fs::remove_dir_all(&target);
                return Err(WorkspaceError::new("internal", format!("could not create branch {wanted}: {error}")));
            }
            branch_name = wanted;
        }
        let workspace = Workspace {
            name: name.to_owned(),
            project_id: project_id.to_owned(),
            path: target.to_string_lossy().into_owned(),
            source: source.to_string_lossy().into_owned(),
            branch: branch_name,
            method: method.to_owned(),
            created_ms: util::now_ms(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&workspace) {
            let _ = util::atomic_write(&Self::meta_path(&target), &bytes);
        }
        Ok(workspace)
    }

    pub fn remove(&self, path: &str) -> Result<(), WorkspaceError> {
        let path = util::normalize(&util::expand_home(path));
        let root = util::canonical_lenient(&self.root);
        let resolved = util::canonical_lenient(&path);
        // Only direct workspace directories (<root>/<project>/<name>) can be removed.
        let inside = resolved.strip_prefix(&root).map(|rel| rel.components().count() == 2).unwrap_or(false);
        if !inside {
            return Err(WorkspaceError::new(
                "invalid_params",
                format!("{} is not a workspace under {}", path.display(), self.root.display()),
            ));
        }
        if !resolved.exists() {
            let _ = std::fs::remove_file(Self::meta_path(&resolved));
            return Err(WorkspaceError::new("not_found", format!("workspace {} does not exist", path.display())));
        }
        std::fs::remove_dir_all(&resolved).map_err(|error| WorkspaceError::new("internal", format!("{}: {error}", resolved.display())))?;
        let _ = std::fs::remove_file(Self::meta_path(&resolved));
        Ok(())
    }

    pub fn list(&self, project_id: Option<&str>) -> Vec<Workspace> {
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.root) else { return out };
        for project in projects.flatten() {
            let project_name = project.file_name().to_string_lossy().into_owned();
            if project_id.is_some_and(|wanted| wanted != project_name) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(project.path()) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".workspace.json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(entry.path())
                    && let Ok(workspace) = serde_json::from_slice::<Workspace>(&bytes)
                    && Path::new(&workspace.path).exists()
                {
                    out.push(workspace);
                }
            }
        }
        out.sort_by_key(|workspace| workspace.created_ms);
        out
    }
}
