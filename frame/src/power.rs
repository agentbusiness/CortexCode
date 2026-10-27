//! Keep-awake: one system sleep assertion held while any holder wants it, refcounted by holder
//! key and released when the last holder releases or its connection closes. The assertion name
//! lists the holders' reasons. macOS: an IOKit power assertion (`PreventUserIdleSystemSleep`);
//! Linux: a `systemd-inhibit` lock (idle and sleep).
//!
//! The frame itself is a holder for as long as it runs ([`FRAME_HOLDER`], taken at startup and
//! released at shutdown), so the machine does not idle-sleep while CortexCode runs, with or without
//! its window. The system drops the assertion with the process if the frame dies without a shutdown.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The frame's own holder: held from startup to shutdown, never by a connection.
pub const FRAME_HOLDER: &str = "frame";
/// The reason the frame's own holder gives (the assertion name in `pmset -g assertions`).
pub const FRAME_REASON: &str = "CortexCode is running";
/// Peer ids start at 1, so 0 belongs to no connection: closing a connection never releases it.
pub const FRAME_PEER: u64 = 0;

struct Holder {
    reason: String,
    peer: u64,
}

struct Inner {
    holders: BTreeMap<String, Holder>,
    assertion: Option<u32>,
}

pub struct KeepAwake {
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeepAwakeState {
    pub asserted: bool,
    pub holders: usize,
    pub assertion_id: u32,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl KeepAwake {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { holders: BTreeMap::new(), assertion: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn hold(&self, holder: &str, reason: &str, peer: u64) -> Result<KeepAwakeState, String> {
        let mut inner = self.lock();
        let reason = if reason.trim().is_empty() { holder.to_owned() } else { reason.trim().to_owned() };
        inner.holders.insert(holder.to_owned(), Holder { reason, peer });
        if let Err(error) = sync(&mut inner) {
            inner.holders.remove(holder);
            return Err(error);
        }
        Ok(state(&inner))
    }

    pub fn release(&self, holder: &str) -> Result<KeepAwakeState, String> {
        let mut inner = self.lock();
        inner.holders.remove(holder);
        sync(&mut inner)?;
        Ok(state(&inner))
    }

    /// Release every holder owned by a closed connection.
    pub fn release_peer(&self, peer: u64) {
        let mut inner = self.lock();
        let before = inner.holders.len();
        inner.holders.retain(|_, holder| holder.peer != peer);
        if inner.holders.len() != before
            && let Err(error) = sync(&mut inner)
        {
            tracing::error!(%error, "keep-awake release failed");
        }
    }

    pub fn release_all(&self) {
        let mut inner = self.lock();
        inner.holders.clear();
        if let Err(error) = sync(&mut inner) {
            tracing::error!(%error, "keep-awake release failed");
        }
    }
}

fn state(inner: &Inner) -> KeepAwakeState {
    KeepAwakeState {
        asserted: inner.assertion.is_some(),
        holders: inner.holders.len(),
        assertion_id: inner.assertion.unwrap_or(0),
    }
}

fn assertion_name(inner: &Inner) -> String {
    let mut reasons: Vec<&str> = Vec::new();
    for holder in inner.holders.values() {
        if !reasons.contains(&holder.reason.as_str()) {
            reasons.push(&holder.reason);
        }
    }
    format!("CortexCode: {}", reasons.join("; "))
}

/// Bring the system assertion in line with the holder set.
fn sync(inner: &mut Inner) -> Result<(), String> {
    match (inner.holders.is_empty(), inner.assertion) {
        (true, Some(id)) => {
            inner.assertion = None;
            os::release(id)?;
            tracing::info!(id, "keep-awake assertion released");
            Ok(())
        }
        (false, None) => {
            let name = assertion_name(inner);
            let id = os::create(&name)?;
            tracing::info!(id, %name, "keep-awake assertion created");
            inner.assertion = Some(id);
            Ok(())
        }
        (false, Some(id)) => {
            match os::rename(id, &assertion_name(inner)) {
                Ok(renamed) => inner.assertion = Some(renamed),
                Err(error) => tracing::warn!(%error, "could not rename the keep-awake assertion"),
            }
            Ok(())
        }
        (true, None) => Ok(()),
    }
}

#[cfg(target_os = "macos")]
mod os {
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::string::{CFString, CFStringRef};

    type IOPMAssertionID = u32;
    type IOReturn = i32;
    const IOPM_ASSERTION_LEVEL_ON: u32 = 255;
    const IO_RETURN_SUCCESS: IOReturn = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: u32,
            name: CFStringRef,
            assertion_id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
        fn IOPMAssertionSetProperty(assertion_id: IOPMAssertionID, key: CFStringRef, value: CFTypeRef) -> IOReturn;
    }

    pub fn create(name: &str) -> Result<u32, String> {
        let kind = CFString::from_static_string("PreventUserIdleSystemSleep");
        let name = CFString::new(name);
        let mut id: IOPMAssertionID = 0;
        // SAFETY: valid CFStrings for the call's duration and a valid out pointer.
        let rc = unsafe { IOPMAssertionCreateWithName(kind.as_concrete_TypeRef(), IOPM_ASSERTION_LEVEL_ON, name.as_concrete_TypeRef(), &mut id) };
        if rc != IO_RETURN_SUCCESS {
            return Err(format!("IOPMAssertionCreateWithName failed: 0x{rc:x}"));
        }
        Ok(id)
    }

    /// The same assertion under a new name.
    pub fn rename(id: u32, name: &str) -> Result<u32, String> {
        let key = CFString::from_static_string("AssertName");
        let name = CFString::new(name);
        // SAFETY: valid assertion id, key and value for the call's duration.
        let rc =
            unsafe { IOPMAssertionSetProperty(id, key.as_concrete_TypeRef(), name.as_CFTypeRef()) };
        if rc != IO_RETURN_SUCCESS {
            return Err(format!("IOPMAssertionSetProperty failed: 0x{rc:x}"));
        }
        Ok(id)
    }

    pub fn release(id: u32) -> Result<(), String> {
        // SAFETY: releasing an assertion id this process created.
        let rc = unsafe { IOPMAssertionRelease(id) };
        if rc != IO_RETURN_SUCCESS {
            return Err(format!("IOPMAssertionRelease failed: 0x{rc:x}"));
        }
        Ok(())
    }
}

/// `systemd-inhibit ... cat` with a pipe for its input: the lock lasts while `cat` runs, and `cat`
/// ends when the pipe closes, so the lock goes with the frame however it ends. The id is its pid.
#[cfg(not(target_os = "macos"))]
mod os {
    use std::collections::BTreeMap;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    static LOCKS: Mutex<BTreeMap<u32, Child>> = Mutex::new(BTreeMap::new());

    pub fn create(name: &str) -> Result<u32, String> {
        let child = Command::new("systemd-inhibit")
            .args(["--what=idle:sleep", "--who=CortexCode", "--mode=block"])
            .arg(format!("--why={name}"))
            .arg("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("systemd-inhibit cannot run ({error}): this system has no sleep lock for CortexCode"))?;
        let id = child.id();
        LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(id, child);
        Ok(id)
    }

    /// A lock's reason is fixed: a new lock under the new name replaces it.
    pub fn rename(id: u32, name: &str) -> Result<u32, String> {
        let renamed = create(name)?;
        release(id)?;
        Ok(renamed)
    }

    pub fn release(id: u32) -> Result<(), String> {
        let child = LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id);
        if let Some(mut child) = child {
            drop(child.stdin.take());
            child.wait().map_err(|error| format!("systemd-inhibit did not end: {error}"))?;
        }
        Ok(())
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.release_all();
    }
}
