//! This node's key: one Ed25519 key per installation, made on first start. On macOS it is kept in
//! the Keychain, a generic password of the service `<app> mesh identity` whose account is the
//! home folder (two homes are two nodes). A `<home>/mesh/node.key` an older version left is moved
//! into the Keychain once and deleted, so the node id stays the same. On Linux it is kept in the
//! Secret Service (through `secret-tool`, the same service and account as attributes), moved there
//! from the file the same way; only where `secret-tool` is not installed, or the Secret Service
//! will not store it, does the key stay in that file, readable by the owner only (with a warning).
//! A Secret Service that is locked or does not answer is an error when no file holds the key: a
//! new key would be another node.
//!
//! The item is made and read through `/usr/bin/security` ([`crate::keychain`]), so a new version
//! of this plugin (another executable) does not make macOS ask the person again.

use iroh::{PublicKey, SecretKey};
use std::path::{Path, PathBuf};

pub struct Identity {
    pub secret: SecretKey,
}

impl Identity {
    pub fn id(&self) -> PublicKey {
        self.secret.public()
    }

    /// The node id as the others name it.
    pub fn hex(&self) -> String {
        self.id().to_string()
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.secret.sign(message).to_bytes().to_vec()
    }

    /// Load the key, or make and keep one. An error when a kept key cannot be read: a new key
    /// would be another node, so none is made then.
    pub fn load_or_create(home: &Path) -> Result<Identity, String> {
        let file = key_file(home);
        let from_file = std::fs::read_to_string(&file).ok().and_then(|text| parse(&text));
        #[cfg(target_os = "macos")]
        {
            let account = home.display().to_string();
            match keychain::get(&account) {
                Ok(Some(text)) => {
                    let secret = parse(&text).ok_or("the mesh identity in the Keychain is not a key")?;
                    // A file the move left behind (the same key) goes now.
                    if from_file.as_ref().is_some_and(|old| old.to_bytes() == secret.to_bytes()) {
                        let _ = std::fs::remove_file(&file);
                    }
                    Ok(Identity { secret })
                }
                Ok(None) => {
                    let secret = from_file.unwrap_or_else(SecretKey::generate);
                    let text = hex::encode(secret.to_bytes());
                    keychain::set(&account, &text)?;
                    // The file is deleted only once the Keychain gives the same key back.
                    if keychain::get(&account).ok().flatten().as_deref() == Some(text.as_str()) {
                        let _ = std::fs::remove_file(&file);
                    }
                    Ok(Identity { secret })
                }
                // The person denied access, or the Keychain is locked: the old file still works.
                Err(error) => from_file.map(|secret| Identity { secret }).ok_or(error),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let account = home.display().to_string();
            match secret::get(&account) {
                Ok(Some(text)) => {
                    let secret = parse(&text).ok_or("the mesh identity in the Secret Service is not a key")?;
                    // A file the move left behind (the same key) goes now.
                    if from_file.as_ref().is_some_and(|old| old.to_bytes() == secret.to_bytes()) {
                        let _ = std::fs::remove_file(&file);
                    }
                    Ok(Identity { secret })
                }
                Ok(None) => {
                    let secret = from_file.unwrap_or_else(SecretKey::generate);
                    let text = hex::encode(secret.to_bytes());
                    match secret::set(&account, &text) {
                        // The file is deleted only once the Secret Service gives the same key back.
                        Ok(()) if secret::get(&account).ok().flatten().as_deref() == Some(text.as_str()) => {
                            let _ = std::fs::remove_file(&file);
                            Ok(Identity { secret })
                        }
                        Ok(()) => {
                            tracing::warn!("the Secret Service did not give the mesh identity back; it is kept in {} as well", file.display());
                            keep_in_file(home, &file, &secret)?;
                            Ok(Identity { secret })
                        }
                        Err(error) => {
                            tracing::warn!(%error, "the mesh identity cannot be kept in the Secret Service; it is kept in {}", file.display());
                            keep_in_file(home, &file, &secret)?;
                            Ok(Identity { secret })
                        }
                    }
                }
                // No `secret-tool` on this system: the key lives in the file.
                Err(error) if error.missing => {
                    tracing::warn!(%error, "no Secret Service for the mesh identity; it is kept in {}", file.display());
                    let secret = match from_file {
                        Some(secret) => secret,
                        None => {
                            let secret = SecretKey::generate();
                            keep_in_file(home, &file, &secret)?;
                            secret
                        }
                    };
                    Ok(Identity { secret })
                }
                // The Secret Service is locked or does not answer (no session bus): the key may be
                // kept there, so none is made; the old file still works.
                Err(error) => from_file
                    .map(|secret| Identity { secret })
                    .ok_or_else(|| format!("cannot read the mesh identity from the Secret Service: {error}")),
            }
        }
    }
}

/// Write the key to `<home>/mesh/node.key`, readable by the owner only (where no Secret Service
/// keeps it). A file that already holds this key is left as it is.
#[cfg(not(target_os = "macos"))]
fn keep_in_file(home: &Path, file: &Path, secret: &SecretKey) -> Result<(), String> {
    let text = hex::encode(secret.to_bytes());
    if std::fs::read_to_string(file).ok().and_then(|old| parse(&old)).is_some_and(|old| old.to_bytes() == secret.to_bytes()) {
        return Ok(());
    }
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(home));
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(file)
        .and_then(|mut out| out.write_all(text.as_bytes()))
        .map_err(|error| format!("cannot keep the mesh identity in {}: {error}", file.display()))
}

fn parse(text: &str) -> Option<SecretKey> {
    let bytes = hex::decode(text.trim()).ok()?;
    let array = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
    Some(SecretKey::from_bytes(&array))
}

fn key_file(home: &Path) -> PathBuf {
    home.join("mesh").join("node.key")
}

#[cfg(target_os = "macos")]
mod keychain {
    fn service() -> String {
        format!("{} mesh identity", crate::APP)
    }

    /// `None` when there is no such item.
    pub fn get(account: &str) -> Result<Option<String>, String> {
        crate::keychain::get(&service(), account).map_err(|error| format!("cannot read the mesh identity from the Keychain: {error}"))
    }

    pub fn set(account: &str, secret: &str) -> Result<(), String> {
        crate::keychain::set(&service(), account, secret).map_err(|error| format!("cannot keep the mesh identity in the Keychain: {error}"))
    }
}

/// The Secret Service (libsecret), through `secret-tool`: an item whose attributes are the same
/// service and account as the macOS Keychain item. The secret goes on stdin, never on a command
/// line.
#[cfg(not(target_os = "macos"))]
mod secret {
    use std::io::Write;
    use std::process::{Command, Output, Stdio};

    const TOOL: &str = "secret-tool";

    fn service() -> String {
        format!("{} mesh identity", crate::APP)
    }

    /// Why a `secret-tool` call failed: `missing` when the tool itself is not installed (then the
    /// key may live in the file); any other failure leaves the key where it is.
    #[derive(Debug)]
    pub struct Failure {
        pub missing: bool,
        pub message: String,
    }

    impl std::fmt::Display for Failure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    fn other(message: String) -> Failure {
        Failure { missing: false, message }
    }

    /// `secret-tool <command> <args> service <service> account <account>`, with `input` on stdin.
    fn run(command: &str, args: &[&str], account: &str, input: Option<&str>) -> Result<Output, Failure> {
        let service = service();
        let mut child = Command::new(TOOL)
            .arg(command)
            .args(args)
            .args(["service", service.as_str(), "account", account])
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Failure {
                missing: error.kind() == std::io::ErrorKind::NotFound,
                message: format!("cannot run {TOOL} ({error}); install libsecret-tools"),
            })?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or_else(|| other(format!("{TOOL}: no stdin")))?;
            stdin.write_all(input.as_bytes()).map_err(|error| other(format!("{TOOL}: {error}")))?;
        }
        child.wait_with_output().map_err(|error| other(format!("{TOOL}: {error}")))
    }

    fn failed(output: &Output) -> Failure {
        other(format!("{TOOL}: {}", String::from_utf8_lossy(&output.stderr).trim()))
    }

    /// `None` when there is no such item (`lookup` then exits 1 and says nothing).
    pub fn get(account: &str) -> Result<Option<String>, Failure> {
        let output = run("lookup", &[], account, None)?;
        if output.status.success() {
            let text = String::from_utf8(output.stdout).map_err(|_| other("the Secret Service item is not text".to_owned()))?;
            let text = text.trim_end_matches('\n');
            return Ok((!text.is_empty()).then(|| text.to_owned()));
        }
        if output.stderr.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Err(failed(&output))
    }

    /// Make the item, or replace its secret.
    pub fn set(account: &str, secret: &str) -> Result<(), Failure> {
        let label = format!("--label={}", service());
        let output = run("store", &[label.as_str()], account, Some(secret))?;
        if output.status.success() { Ok(()) } else { Err(failed(&output)) }
    }
}

/// Verify an Ed25519 signature by a node id.
pub fn verify(node: &str, message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = node.parse::<PublicKey>() else { return false };
    let Ok(array) = <[u8; 64]>::try_from(signature) else { return false };
    let signature = iroh::Signature::from_bytes(&array);
    key.verify(message, &signature).is_ok()
}
