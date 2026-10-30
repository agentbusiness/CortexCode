//! Generic passwords in the macOS login Keychain, always through Apple's `/usr/bin/security`.
//!
//! An item trusts the program that made it; CortexTerm's own executables are signed anew by every
//! build and version, so an item one of them made would ask the person again after each update.
//! `/usr/bin/security` stays the same program: it makes and reads every item, so nothing
//! asks. The secret goes to it on stdin (`security -i`), never on a command line.

use std::io::Write;
use std::process::{Command, Stdio};

const SECURITY: &str = "/usr/bin/security";
/// `security`'s exit status for "no such item".
const NOT_FOUND: i32 = 44;

/// `None` when there is no such item.
pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
    let output = Command::new(SECURITY)
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {SECURITY}: {error}"))?;
    if output.status.success() {
        let text = String::from_utf8(output.stdout).map_err(|_| "the Keychain item is not text".to_owned())?;
        return Ok(Some(text.trim_end_matches('\n').to_owned()));
    }
    if output.status.code() == Some(NOT_FOUND) {
        return Ok(None);
    }
    Err(format!("Keychain: {}", String::from_utf8_lossy(&output.stderr).trim()))
}

/// Make the item, or replace its secret.
pub fn set(service: &str, account: &str, secret: &str) -> Result<(), String> {
    // One line of `security -i`, each value in quotes: no quote, backslash or control character
    // in them; the secret is plain ASCII, so `find-generic-password -w` gives it back as it is.
    let plain = |text: &str| !text.is_empty() && !text.chars().any(|c| c == '"' || c == '\\' || c.is_control());
    if !plain(service) || !plain(account) {
        return Err("the Keychain item's name has a quote, backslash or control character".to_owned());
    }
    if !plain(secret) || !secret.is_ascii() {
        return Err("only plain ASCII text without quotes or backslashes can be kept in the Keychain".to_owned());
    }
    let mut child = Command::new(SECURITY)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {SECURITY}: {error}"))?;
    let line = format!("add-generic-password -U -s \"{service}\" -a \"{account}\" -w \"{secret}\"\n");
    child.stdin.take().ok_or("no stdin")?.write_all(line.as_bytes()).map_err(|error| format!("Keychain: {error}"))?;
    let output = child.wait_with_output().map_err(|error| format!("Keychain: {error}"))?;
    let errors = String::from_utf8_lossy(&output.stderr);
    // `security -i` exits 0 even when a command fails; a failure is written to stderr.
    if !output.status.success() || !errors.trim().is_empty() {
        return Err(format!("Keychain: {}", errors.trim()));
    }
    Ok(())
}

/// Remove the item; no such item is not an error.
pub fn delete(service: &str, account: &str) -> Result<(), String> {
    let output = Command::new(SECURITY)
        .args(["delete-generic-password", "-s", service, "-a", account])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {SECURITY}: {error}"))?;
    if output.status.success() || output.status.code() == Some(NOT_FOUND) {
        return Ok(());
    }
    Err(format!("Keychain: {}", String::from_utf8_lossy(&output.stderr).trim()))
}
