//! The baseline check (`tools.tests.baseline`): a session's test command run once more on the code
//! as it was before its running turn changed it. Tests that pass there too do not check the change
//! (seen: tests written for the code passed on wrong code, nested tests never ran, and the fix was
//! taken as done). Each code file the turn's `edit`s changed is put back as it was, test files stay
//! as they are now, the command runs in a one-off shell with the session's environment, and every
//! file is restored, also when the run fails or is cancelled.

use crate::core::Core;
use crate::shell::RunRequest;
use crate::util;
use cc_protocol::RpcError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Output read for the test counts.
const OUTPUT_BYTES: u64 = 1 << 20;

/// Code files put back as they were before the turn; dropping it restores them as they are now.
struct Swapped<'a> {
    core: &'a Core,
    workspace: &'a Path,
    /// Each file and its content now (None: it does not exist now).
    now: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl<'a> Swapped<'a> {
    /// Put each file back as `before` says (None: the turn created it, so it is taken away).
    fn apply(core: &'a Core, workspace: &'a Path, before: &[(PathBuf, Option<String>)]) -> std::io::Result<Swapped<'a>> {
        let mut swapped = Swapped { core, workspace, now: Vec::with_capacity(before.len()) };
        for (path, original) in before {
            swapped.now.push((path.clone(), util::read_host_file(path).ok()));
            match original {
                Some(text) => util::atomic_write_checked(path, text.as_bytes(), |path| util::is_inside(path, workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(workspace, path))?,
                None => match util::remove_file_checked(path, |path| util::is_inside(path, workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(workspace, path)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                },
            }
        }
        Ok(swapped)
    }
}

impl Drop for Swapped<'_> {
    fn drop(&mut self) {
        for (path, now) in &self.now {
            let restored = match now {
                Some(bytes) => util::atomic_write_checked(path, bytes, |path| util::is_inside(path, self.workspace) && !self.core.sandbox.protected(path) && !self.core.sandbox.git_control_write_denied(self.workspace, path)),
                None => util::remove_file_checked(path, |path| util::is_inside(path, self.workspace) && !self.core.sandbox.protected(path) && !self.core.sandbox.git_control_write_denied(self.workspace, path)).or_else(|error| if error.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(error) }),
            };
            if let Err(error) = restored {
                tracing::error!(path = %path.display(), %error, "restoring a file after the baseline check failed");
            }
        }
    }
}

/// The code files (not test files, not the scratch folder) the session's running turn changed, each
/// with its content before the turn.
fn changed_code(core: &Core, session_id: &str, workspace: &Path) -> Vec<(PathBuf, Option<String>)> {
    let scratch = util::scratch_dir(workspace);
    let Ok(originals) = core.originals.lock() else { return Vec::new() };
    originals
        .get(session_id)
        .into_iter()
        .flatten()
        .filter(|(path, _)| util::is_inside(path, workspace) && !util::is_inside(path, &scratch))
        .filter(|(path, _)| !util::is_test_path(path.strip_prefix(workspace).unwrap_or(path)))
        .map(|(path, before)| (path.clone(), before.clone()))
        .collect()
}

/// `tools.tests.baseline`.
pub async fn check(
    core: &Arc<Core>,
    host: &dyn crate::host::Host,
    session_id: &str,
    workspace: &str,
    command: &str,
    timeout_ms: Option<u64>,
    cancel: CancellationToken,
) -> Result<Value, RpcError> {
    if session_id.is_empty() || workspace.is_empty() || command.trim().is_empty() {
        return Err(RpcError::invalid_params("tools.tests.baseline needs session_id, workspace and command"));
    }
    let root = util::canonical_lenient(&util::expand_home(workspace));
    let before = changed_code(core, session_id, &root);
    if before.is_empty() {
        return Ok(json!({ "checked": false, "reason": "the turn changed no code file" }));
    }
    let context = crate::service::host_context(workspace, None, Some(session_id));
    let session = crate::exec::Session { workspace: root.clone(), context: &context, python_env: true };
    let runtime =
        core.runtime(host, session_id, &root, true).await.map_err(|message| RpcError::new(cc_protocol::codes::UNAVAILABLE, message))?;
    let mut identity = crate::exec::identity(core, &session, Some(&runtime));
    // A one-off shell that starts where the session's shell is (its directory and environment).
    let temporary = format!("{session_id}-baseline-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    core.shells.copy_state(session_id, &temporary).map_err(|error| RpcError::internal(error.to_string()))?;
    identity.session_id = temporary.clone();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(core.config.default_timeout_ms));
    let run = {
        let _swapped = Swapped::apply(core, &root, &before).map_err(|error| RpcError::internal(format!("cannot put the code back as it was: {error}")))?;
        core.shells
            .run(&identity, RunRequest { command, cwd: None, env: Vec::new(), toolchain: crate::shell::Toolchain::default(), timeout }, cancel, |_: &[u8]| {})
            .await
    };
    core.shells.drop_session(&temporary).await;
    let run = run.map_err(|error| RpcError::internal(format!("could not run: {error}")))?;
    let whole = run.output.whole(OUTPUT_BYTES).unwrap_or_default();
    let tests = crate::testout::summary(&whole);
    Ok(json!({
        "checked": true,
        "files": before.len(),
        "exit_code": run.exit_code,
        "timed_out": run.timed_out,
        "ran": tests.as_ref().map(|tests| tests.ran),
        "failed": tests.as_ref().map(|tests| tests.failed),
    }))
}
