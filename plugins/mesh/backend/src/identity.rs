//! This node's key: one Ed25519 key per installation, made on first start. On macOS it is kept in
//! the Keychain, a generic password of the service `<app> mesh identity` whose account is the
//! home folder (two homes are two nodes). A `<home>/mesh/node.key` an older version left is moved
//! into the Keychain once and deleted, so the node id stays the same. Elsewhere the key stays in
//! that file, readable by the owner only.
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
            if let Some(secret) = from_file {
                return Ok(Identity { secret });
            }
            let secret = SecretKey::generate();
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(home));
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&file)
                .and_then(|mut out| out.write_all(hex::encode(secret.to_bytes()).as_bytes()))
                .map_err(|error| format!("cannot keep the mesh identity in {}: {error}", file.display()))?;
            Ok(Identity { secret })
        }
    }
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

/// Verify an Ed25519 signature by a node id.
pub fn verify(node: &str, message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = node.parse::<PublicKey>() else { return false };
    let Ok(array) = <[u8; 64]>::try_from(signature) else { return false };
    let signature = iroh::Signature::from_bytes(&array);
    key.verify(message, &signature).is_ok()
}
