//! The cortex: a signed, append-only membership log every member holds (`<home>/mesh/members.jsonl`).
//! Each entry names what changed (a member added or renamed, its addresses, a member revoked), who signed
//! it and the hash of the entry before it. A node is a member when a valid chain says so; a
//! connection from a key the chain does not hold is refused. No server can add a node: only a
//! member's key can.

use crate::identity::{Identity, verify};
use cc_protocol::contracts::mesh::Member;
use cc_protocol::contracts::now_ms;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub seq: u64,
    /// Hex SHA-256 of the entry before this one (empty for the first).
    pub prev: String,
    /// `add` | `rename` | `revoke` | `addrs`
    pub kind: String,
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub addrs: Vec<String>,
    /// The signing member (hex).
    pub by: String,
    pub ts_ms: u64,
    /// Hex Ed25519 signature by `by` over the entry without this field.
    #[serde(default)]
    pub sig: String,
}

impl Entry {
    fn signed_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.sig = String::new();
        serde_json::to_vec(&unsigned).unwrap_or_default()
    }

    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(serde_json::to_vec(self).unwrap_or_default()))
    }
}

#[derive(Default)]
pub struct Log {
    pub entries: Vec<Entry>,
}

impl Log {
    pub fn load(path: &Path) -> Log {
        let mut log = Log::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str::<Entry>(line) {
                    log.entries.push(entry);
                }
            }
        }
        if let Err(error) = log.validate() {
            tracing::warn!(%error, "the membership log is not valid; starting from the valid prefix");
        }
        log
    }

    pub fn save(&self, path: &Path) {
        let mut text = String::new();
        for entry in &self.entries {
            if let Ok(line) = serde_json::to_string(entry) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// The members as the chain stands: a map from node id to the member.
    pub fn members(&self) -> BTreeMap<String, Member> {
        let mut members: BTreeMap<String, Member> = BTreeMap::new();
        for entry in &self.entries {
            match entry.kind.as_str() {
                "add" => {
                    members.insert(entry.node.clone(), Member { node: entry.node.clone(), name: entry.name.clone(), added_ms: entry.ts_ms, by: entry.by.clone(), addrs: entry.addrs.clone() });
                }
                "rename" => {
                    if let Some(member) = members.get_mut(&entry.node) {
                        member.name = entry.name.clone();
                    }
                }
                "addrs" => {
                    if let Some(member) = members.get_mut(&entry.node) {
                        member.addrs = entry.addrs.clone();
                    }
                }
                "revoke" => {
                    members.remove(&entry.node);
                }
                _ => {}
            }
        }
        members
    }

    pub fn is_member(&self, node: &str) -> bool {
        self.members().contains_key(node)
    }

    pub fn member(&self, node: &str) -> Option<Member> {
        self.members().remove(node)
    }

    /// Check the chain: hashes link, every entry is signed by a member of the chain so far (the
    /// first by itself), and truncate to the valid prefix.
    pub fn validate(&mut self) -> Result<(), String> {
        let mut members: BTreeMap<String, ()> = BTreeMap::new();
        let mut prev = String::new();
        let mut valid = 0;
        let mut problem = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.prev != prev || entry.seq != index as u64 + 1 {
                problem = Some(format!("entry {} does not follow the one before it", entry.seq));
                break;
            }
            let signer_ok = if index == 0 { entry.kind == "add" && entry.by == entry.node } else { members.contains_key(&entry.by) };
            if !signer_ok {
                problem = Some(format!("entry {} is signed by {}, not a member", entry.seq, entry.by));
                break;
            }
            let Ok(sig) = hex::decode(&entry.sig) else {
                problem = Some(format!("entry {} has no readable signature", entry.seq));
                break;
            };
            if !verify(&entry.by, &entry.signed_bytes(), &sig) {
                problem = Some(format!("entry {}'s signature does not verify", entry.seq));
                break;
            }
            match entry.kind.as_str() {
                "add" => {
                    members.insert(entry.node.clone(), ());
                }
                "revoke" => {
                    members.remove(&entry.node);
                }
                _ => {}
            }
            prev = entry.hash();
            valid = index + 1;
        }
        self.entries.truncate(valid);
        match problem {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Append an entry signed by this node.
    pub fn append(&mut self, me: &Identity, kind: &str, node: &str, name: &str, addrs: Vec<String>) -> Entry {
        let prev = self.entries.last().map(Entry::hash).unwrap_or_default();
        let mut entry = Entry {
            seq: self.entries.len() as u64 + 1,
            prev,
            kind: kind.to_owned(),
            node: node.to_owned(),
            name: name.to_owned(),
            addrs,
            by: me.hex(),
            ts_ms: now_ms(),
            sig: String::new(),
        };
        entry.sig = hex::encode(me.sign(&entry.signed_bytes()));
        self.entries.push(entry.clone());
        entry
    }

    /// Take a member's copy of the log: when it is a valid chain that extends ours (same prefix),
    /// ours becomes it; when ours is longer on the same prefix, nothing changes; a different
    /// prefix is another cortex and is refused.
    pub fn merge(&mut self, theirs: Vec<Entry>) -> Result<bool, String> {
        let mut candidate = Log { entries: theirs };
        candidate.validate()?;
        let shared = self.entries.len().min(candidate.entries.len());
        if self.entries[..shared] != candidate.entries[..shared] {
            return Err("the other node belongs to a different cortex".to_owned());
        }
        if candidate.entries.len() > self.entries.len() {
            self.entries = candidate.entries;
            return Ok(true);
        }
        Ok(false)
    }

    /// The first member (the founder) made this node's own cortex.
    pub fn founded(&self) -> bool {
        !self.entries.is_empty()
    }
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("mesh").join("members.jsonl")
}
