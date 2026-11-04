//! The cortex: a signed, append-only membership log every member holds (kept in the app's store as
//! stream `mesh.members`, `crate::store`).
//! Each entry names what changed (a member added or renamed, its addresses, a member revoked), who signed
//! it and the hash of the entry before it. A node is a member when a valid chain says so; a
//! connection from a key the chain does not hold is refused. No server can add a node: only a
//! member's key can.

use crate::identity::{Identity, verify};
use cc_protocol::contracts::mesh::Member;
use cc_protocol::contracts::now_ms;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub seq: u64,
    /// Hex SHA-256 of the entry before this one (empty for the first).
    pub prev: String,
    /// `add` | `rename` | `revoke` | `addrs` | `link` (`node`: the id of a network taken in) |
    /// `machine` (`node`: a computer's machine key, which may then sign its own apps in)
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
    /// A log an earlier version kept as a file (one entry per line), read once to move it into the
    /// store.
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

    /// The members as the chain stands: a map from node id to the member.
    pub fn members(&self) -> BTreeMap<String, Member> {
        let mut members: BTreeMap<String, Member> = BTreeMap::new();
        for entry in &self.entries {
            match entry.kind.as_str() {
                "add" => {
                    members.insert(entry.node.clone(), Member { node: entry.node.clone(), name: entry.name.clone(), added_ms: entry.ts_ms, by: entry.by.clone(), addrs: entry.addrs.clone(), local: false });
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
                "add" | "machine" => {
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

    /// Take a member's copy of the log. The same chain: the longer copy is kept. Two copies that
    /// went apart after a common start (two members wrote at the same moment): the copy whose first
    /// differing entry hashes lower is kept, on every member alike, and what the other copy says
    /// that it does not is said again on top of it, signed by `me` (`Log::replay`), so no member,
    /// computer, removal or link is lost and every member ends on one chain. While `me` is not a
    /// member of the kept copy, this copy stays as it is: a member of both says it again and
    /// pushes it. A copy with another first entry is another network and is refused.
    pub fn merge(&mut self, me: &Identity, theirs: Vec<Entry>) -> Result<bool, String> {
        let mut candidate = Log { entries: theirs };
        candidate.validate()?;
        let shared = self.entries.iter().zip(&candidate.entries).take_while(|(ours, theirs)| ours == theirs).count();
        if shared == 0 && !self.entries.is_empty() && !candidate.entries.is_empty() {
            return Err("the other node belongs to a different cortex".to_owned());
        }
        if shared == self.entries.len() {
            // Ours is theirs, or the beginning of it.
            if candidate.entries.len() > self.entries.len() {
                self.entries = candidate.entries;
                return Ok(true);
            }
            return Ok(false);
        }
        if shared == candidate.entries.len() {
            // Theirs is the beginning of ours.
            return Ok(false);
        }
        let node = me.hex();
        if candidate.entries[shared].hash() < self.entries[shared].hash() {
            if !candidate.is_member(&node) {
                return Ok(false);
            }
            let lost = Log { entries: std::mem::take(&mut self.entries) };
            candidate.replay(me, &lost, shared);
            self.entries = candidate.entries;
            return Ok(true);
        }
        if !self.is_member(&node) {
            return Ok(false);
        }
        let before = self.entries.len();
        self.replay(me, &candidate, shared);
        Ok(self.entries.len() > before)
    }

    /// Say again on top of this chain what `other` says after its first `from` entries and this
    /// chain does not: computers, members, removals, names, addresses and links, each signed by
    /// `me`. What this chain already says is skipped, and a member this chain removed stays out,
    /// so replaying the same copy twice adds nothing the second time. A removal said again is
    /// signed by `me`, so it counts as removed even where the lost entry was a leave.
    pub fn replay(&mut self, me: &Identity, other: &Log, from: usize) {
        let (their_members, their_machines) = (other.members(), other.machines());
        for entry in other.entries.iter().skip(from) {
            let node = entry.node.as_str();
            match entry.kind.as_str() {
                "machine" => {
                    if their_machines.contains_key(node) && !self.machines().contains_key(node) && !self.removed(node) {
                        self.append(me, "machine", node, &entry.name, Vec::new());
                    }
                }
                "add" => {
                    if let Some(member) = their_members.get(node)
                        && !self.is_member(node)
                        && !self.removed(node)
                    {
                        self.append(me, "add", node, &member.name, member.addrs.clone());
                    }
                }
                "revoke" => {
                    let gone_there = !their_members.contains_key(node) && !their_machines.contains_key(node);
                    if gone_there && (self.is_member(node) || self.machines().contains_key(node)) {
                        self.append(me, "revoke", node, &entry.name, Vec::new());
                    }
                }
                "rename" => {
                    if let (Some(theirs), Some(ours)) = (their_members.get(node), self.members().get(node))
                        && theirs.name != ours.name
                    {
                        self.append(me, "rename", node, &theirs.name, Vec::new());
                    }
                }
                "addrs" => {
                    if let (Some(theirs), Some(ours)) = (their_members.get(node), self.members().get(node))
                        && theirs.addrs != ours.addrs
                    {
                        self.append(me, "addrs", node, "", theirs.addrs.clone());
                    }
                }
                "link" => {
                    if !self.linked(node) {
                        self.append(me, "link", node, "", Vec::new());
                    }
                }
                _ => {}
            }
        }
    }

    /// The network this chain is: the hash of its first entry (`Mesh::network` shows its first ten
    /// characters).
    pub fn id(&self) -> Option<String> {
        self.entries.first().map(Entry::hash)
    }

    /// Whether this chain took in the network `id` (a `link` entry naming it).
    pub fn linked(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.kind == "link" && entry.node == id)
    }

    /// Take in the network `old` this node was in: a `link` entry naming it, then its members
    /// (`Log::carry_over`). Its members move onto this chain when it reaches them
    /// (`Mesh::log_received`), so joining a second network links the two instead of leaving one.
    pub fn link(&mut self, me: &Identity, old: &Log) {
        let Some(id) = old.id() else { return };
        if self.id().as_deref() == Some(id.as_str()) {
            return;
        }
        if !self.linked(&id) {
            self.append(me, "link", &id, "", Vec::new());
        }
        self.carry_over(me, old);
    }

    /// A `machine` for each computer of `old` this chain does not hold, then an `add` for each of
    /// its members; one this chain revoked stays out (a member removed it, or it left).
    pub fn carry_over(&mut self, me: &Identity, old: &Log) {
        let machines = self.machines();
        for (key, name) in old.machines() {
            if !machines.contains_key(&key) && !self.removed(&key) {
                self.append(me, "machine", &key, &name, Vec::new());
            }
        }
        let held = self.members();
        let revoked: BTreeSet<String> = self.entries.iter().filter(|entry| entry.kind == "revoke").map(|entry| entry.node.clone()).collect();
        for (node, member) in old.members() {
            if !held.contains_key(&node) && !revoked.contains(&node) {
                self.append(me, "add", &node, &member.name, member.addrs.clone());
            }
        }
    }

    /// The computers in this chain: machine key -> the computer's name. Every app on a computer
    /// keeps its machine key (`crate::machine`); a machine key signs its own apps in.
    pub fn machines(&self) -> BTreeMap<String, String> {
        let mut machines = BTreeMap::new();
        for entry in &self.entries {
            match entry.kind.as_str() {
                "machine" => {
                    machines.insert(entry.node.clone(), entry.name.clone());
                }
                "revoke" => {
                    machines.remove(&entry.node);
                }
                _ => {}
            }
        }
        machines
    }

    /// Whether a member took `node` out (it comes back only by an invitation). A node that left
    /// was revoked by itself or by its computer's machine key, and is not removed.
    pub fn removed(&self, node: &str) -> bool {
        let machine_keys: BTreeSet<&str> = self.entries.iter().filter(|entry| entry.kind == "machine").map(|entry| entry.node.as_str()).collect();
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.node == node && entry.kind != "link")
            .is_some_and(|entry| entry.kind == "revoke" && entry.by != node && !machine_keys.contains(entry.by.as_str()))
    }

    /// A member and everything of its computer, apps first and machine keys last: the machine
    /// keys that signed it in or that it brought in, and every member those keys signed in or
    /// that brought them in. Removing or leaving takes the whole computer out, or its machine key
    /// would sign its apps straight back in.
    pub fn computer_of(&self, node: &str) -> Vec<String> {
        let members = self.members();
        let machines = self.machines();
        let brought = |key: &str, by: &str| self.entries.iter().any(|entry| entry.kind == "machine" && entry.node == key && entry.by == by);
        let keys: Vec<String> = machines.keys().filter(|key| members.get(node).is_some_and(|member| &member.by == *key) || brought(key, node)).cloned().collect();
        let mut apps = vec![node.to_owned()];
        for (app, member) in &members {
            if app != node && keys.iter().any(|key| &member.by == key || brought(key, app)) {
                apps.push(app.clone());
            }
        }
        apps.extend(keys);
        apps
    }

    /// Fold in the computer's shared log (`crate::machine`): the longer copy of this chain; a
    /// network that linked this one in, or that this computer is in (its machine key is in it),
    /// with this app signed in by the machine key when it is not a member and this app's own
    /// network linked in. Nothing changes for a copy of a network this chain already linked in, a
    /// fork, or a network this computer is not in. True when this chain changed.
    pub fn absorb(&mut self, me: &Identity, machine: &Identity, file: Log, name: &str, addrs: &[String]) -> bool {
        let Some(theirs) = file.id() else { return false };
        let node = me.hex();
        let mine = self.id();
        if mine.as_deref() == Some(theirs.as_str()) {
            let mut changed = self.merge(me, file.entries).unwrap_or(false);
            changed |= self.sign_in(machine, &node, name, addrs);
            return changed;
        }
        if let Some(mine) = &mine
            && self.linked(&theirs)
            && !(file.linked(mine) && theirs < *mine)
        {
            return false;
        }
        let mut candidate = file;
        if !candidate.sign_in(machine, &node, name, addrs) && !candidate.is_member(&node) {
            return false;
        }
        if self.members().len() > 1 {
            candidate.link(me, self);
        }
        *self = candidate;
        true
    }

    /// Sign this app in with its computer's machine key: when the key is in this chain, the app is
    /// not a member, and no member took the app out. True when an `add` was made.
    fn sign_in(&mut self, machine: &Identity, node: &str, name: &str, addrs: &[String]) -> bool {
        if self.is_member(node) || !self.machines().contains_key(&machine.hex()) || self.removed(node) {
            return false;
        }
        self.append(machine, "add", node, name, addrs.to_vec());
        true
    }

    /// Bring this app's computer into its network: a `machine` entry for the machine key, signed
    /// by this app, once this app is a member of a network with others and the key is not in it.
    /// A computer a member removed is not brought back. True when the entry was made.
    pub fn bring_machine(&mut self, me: &Identity, machine: &Identity, name: &str) -> bool {
        let key = machine.hex();
        if self.members().len() < 2 || !self.is_member(&me.hex()) || self.machines().contains_key(&key) || self.removed(&key) {
            return false;
        }
        self.append(me, "machine", &key, name, Vec::new());
        true
    }

    /// The first member (the founder) made this node's own cortex.
    pub fn founded(&self) -> bool {
        !self.entries.is_empty()
    }
}

/// Where an earlier version kept the log.
pub fn log_path(home: &Path) -> PathBuf {
    home.join("mesh").join("members.jsonl")
}
