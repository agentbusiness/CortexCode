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
//!
//! The machine key of the cortex-network ([`Identity::machine`], `crate::machine`) is kept the
//! same way under its own service, `cortex-network machine`, with the person's home folder as the
//! account, so every app of this person on this computer reads the same key.

use iroh::{PublicKey, SecretKey};
use std::path::{Path, PathBuf};

/// The Keychain (Secret Service) service of the machine key every app on this computer shares.
const MACHINE_SERVICE: &str = "cortex-network machine";

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
        load_or_create_kept(&format!("{} mesh identity", crate::APP), &home.display().to_string(), &key_file(home))
    }

    /// The machine key every app of this person on this computer shares (`crate::machine`):
    /// kept like a node's key, under `cortex-network machine` with the home folder as the account
    /// (where no Secret Service keeps it: a file in the cortex-network folder, readable by the
    /// owner only). Read back once more after it is made, so two apps that start together and
    /// both make one end up holding the one the store kept.
    pub fn machine() -> Result<Identity, String> {
        let home = std::env::var_os("HOME").ok_or("no home folder")?;
        let dir = crate::machine::private_dir().ok_or("the cortex-network folder is not this person's alone")?;
        let account = PathBuf::from(home).display().to_string();
        let file = dir.join("machine.key");
        load_or_create_kept(MACHINE_SERVICE, &account, &file)?;
        load_or_create_kept(MACHINE_SERVICE, &account, &file)
    }
}

/// A small secret of the cortex-network kept beside the keys (`crate::machine`'s rollback
/// guard); `None` when there is none or no secret store answers.
pub fn kept_get(service: &str, account: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        crate::keychain::get(service, account).ok().flatten()
    }
    #[cfg(not(target_os = "macos"))]
    {
        secret::get(service, account).ok().flatten()
    }
}

/// Keep a small secret of the cortex-network (plain ASCII, no quotes or backslashes); false when
/// no secret store took it.
pub fn kept_set(service: &str, account: &str, text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::keychain::set(service, account, text).is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        secret::set(service, account, text).is_ok()
    }
}

/// A key kept under `service` and `account`, or made and kept there; `file` is where an older
/// version left it, or where it lives when no Secret Service keeps it. An error when a kept key
/// cannot be read: a new key would be another identity, so none is made then.
fn load_or_create_kept(service: &str, account: &str, file: &Path) -> Result<Identity, String> {
    let file = file.to_path_buf();
    let from_file = std::fs::read_to_string(&file).ok().and_then(|text| parse(&text));
    #[cfg(target_os = "macos")]
    {
        let account = account.to_owned();
        match keychain::get(service, &account) {
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
                keychain::set(service, &account, &text)?;
                // The file is deleted only once the Keychain gives the same key back.
                if keychain::get(service, &account).ok().flatten().as_deref() == Some(text.as_str()) {
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
        let account = account.to_owned();
        match secret::get(service, &account) {
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
                match secret::set(service, &account, &text) {
                    // The file is deleted only once the Secret Service gives the same key back.
                    Ok(()) if secret::get(service, &account).ok().flatten().as_deref() == Some(text.as_str()) => {
                        let _ = std::fs::remove_file(&file);
                        Ok(Identity { secret })
                    }
                    Ok(()) => {
                        tracing::warn!("the Secret Service did not give the mesh identity back; it is kept in {} as well", file.display());
                        keep_in_file(&file, &secret)?;
                        Ok(Identity { secret })
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the mesh identity cannot be kept in the Secret Service; it is kept in {}", file.display());
                        keep_in_file(&file, &secret)?;
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
                        keep_in_file(&file, &secret)?;
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

/// Write the key to its file, readable by the owner only (where no Secret Service keeps it). A
/// file that already holds this key is left as it is.
#[cfg(not(target_os = "macos"))]
fn keep_in_file(file: &Path, secret: &SecretKey) -> Result<(), String> {
    let text = hex::encode(secret.to_bytes());
    if std::fs::read_to_string(file).ok().and_then(|old| parse(&old)).is_some_and(|old| old.to_bytes() == secret.to_bytes()) {
        return Ok(());
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
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
    /// `None` when there is no such item.
    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        crate::keychain::get(service, account).map_err(|error| format!("cannot read the mesh identity from the Keychain: {error}"))
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<(), String> {
        crate::keychain::set(service, account, secret).map_err(|error| format!("cannot keep the mesh identity in the Keychain: {error}"))
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
    fn run(command: &str, args: &[&str], service: &str, account: &str, input: Option<&str>) -> Result<Output, Failure> {
        let mut child = Command::new(TOOL)
            .arg(command)
            .args(args)
            .args(["service", service, "account", account])
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
    pub fn get(service: &str, account: &str) -> Result<Option<String>, Failure> {
        let output = run("lookup", &[], service, account, None)?;
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
    pub fn set(service: &str, account: &str, secret: &str) -> Result<(), Failure> {
        let label = format!("--label={service}");
        let output = run("store", &[label.as_str()], service, account, Some(secret))?;
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
