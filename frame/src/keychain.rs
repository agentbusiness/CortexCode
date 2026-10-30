//! The frame's API token in the macOS Keychain: a generic password of the service [`SERVICE`]
//! whose account is the home folder (each home has its own frame). A new token is made at every
//! start and written over the last one; the item outlives the frame, holding a token that no
//! longer opens anything. Scripts read it with
//! `security find-generic-password -s "CortexCode frame token" -a <home> -w`; the shell and the
//! mesh get it in their environment.
//!
//! Every item is a generic password in the login Keychain, always through Apple's `/usr/bin/security`.
//!
//! An item trusts the program that made it; CortexCode's own executables are signed anew by every
//! build and version, so an item one of them made would ask the person again after each update.
//! `/usr/bin/security` stays the same program: it makes and reads every item, so nothing
//! asks. The secret goes to it on stdin (`security -i`), never on a command line.

use std::io::Write;
use std::process::{Command, Stdio};

const SECURITY: &str = "/usr/bin/security";

pub const SERVICE: &str = "CortexCode frame token";

/// Write the token (in the background: a locked Keychain asks the person, and the start does
/// not wait for that).
pub fn store(home: &std::path::Path, token: &str) {
    let account = home.display().to_string();
    let token = token.to_owned();
    std::thread::spawn(move || {
        if let Err(error) = set(SERVICE, &account, &token) {
            tracing::warn!(%error, "cannot keep the API token in the Keychain; scripts cannot reach the frame");
        }
    });
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
