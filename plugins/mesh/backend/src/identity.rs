//! This node's key: one Ed25519 key per installation, made on first start and kept in
//! `<home>/mesh/node.key`, readable by the owner only (like an SSH key: the home folder is the
//! person's, and a file asks nobody; the Keychain would ask the person again at every version of
//! this plugin, since each is another executable). The public key is the node's id and address.

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

    /// Load the key, or make and keep one.
    pub fn load_or_create(home: &Path) -> Identity {
        let file = key_file(home);
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(bytes) = hex::decode(text.trim())
            && bytes.len() == 32
        {
            let mut array = [0_u8; 32];
            array.copy_from_slice(&bytes);
            return Identity { secret: SecretKey::from_bytes(&array) };
        }
        let secret = SecretKey::generate();
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(home));
        if std::fs::write(&file, hex::encode(secret.to_bytes())).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
        Identity { secret }
    }
}

fn key_file(home: &Path) -> PathBuf {
    home.join("mesh").join("node.key")
}

/// Verify an Ed25519 signature by a node id.
pub fn verify(node: &str, message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = node.parse::<PublicKey>() else { return false };
    let Ok(array) = <[u8; 64]>::try_from(signature) else { return false };
    let signature = iroh::Signature::from_bytes(&array);
    key.verify(message, &signature).is_ok()
}
