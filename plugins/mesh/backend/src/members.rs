//! The cortex: a signed, append-only membership log every member holds (kept in the app's store as
//! stream `mesh.members`, `crate::store`).
//! Each entry names what changed (a member added or renamed, its addresses, a member revoked), who signed
//! it and the hash of the entry before it. A node is a member when a valid chain says so; a
//! connection from a key the chain does not hold is refused. No server can add a node: only a
//! member's key can.
//!
//! A removal takes effect from a point of the chain. A `revoke` names the entry it follows
//! (`after`: the head of the copy it was made on), and an entry said again on another copy
//! (`Log::replay`) names the key that first said it (`origin`). What a removed key said after
//! the point its removal follows has no effect, on any copy, and neither has anything that
//! rests on it: whoever it admitted there is not a member, and what those said counts for
//! nothing (`Log::in_effect`). What it said up to that point stays.

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
    /// On a `revoke`: the hex hash of the entry of this chain the removal follows. What `node`
    /// said after that entry has no effect. A removal made on a chain's head names the head;
    /// one said again on another copy names the last entry the two copies share, or the head
    /// when that copy holds nothing of `node`'s the removal had not seen. Left out of the bytes
    /// when empty: a removal written before this field follows the entry before it (`prev`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub after: String,
    /// The key that first said this (hex), when `by` says it again on another copy
    /// (`Log::replay`). Left out of the bytes when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
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

    /// The key that first said this entry: `origin` when another member said it again, else the
    /// signer.
    pub fn author(&self) -> &str {
        if self.origin.is_empty() { &self.by } else { &self.origin }
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
        for entry in self.effective() {
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

    /// Which entries are in effect, one flag per entry. An entry is in effect when its signer,
    /// and the key that first said it (`Entry::author`), are members or computers of the chain
    /// as the entries in effect before it leave it, and no removal in effect later in the chain
    /// takes its author out without following it: a `revoke` of a key cuts every entry that key
    /// first said after the entry the removal follows (`Entry::after`) and before the removal.
    /// The first entry is always in effect. The removals are taken from the end of the chain to
    /// its start, and an entry once cut stays cut.
    pub fn in_effect(&self) -> Vec<bool> {
        let mut cut = vec![false; self.entries.len()];
        loop {
            let effect = self.pass(&cut);
            let mut more = false;
            for index in (1..self.entries.len()).rev() {
                let entry = &self.entries[index];
                if !effect[index] || entry.kind != "revoke" {
                    continue;
                }
                for at in self.followed(index) + 1..index {
                    if !cut[at] && self.entries[at].author() == entry.node {
                        cut[at] = true;
                        more = true;
                    }
                }
                if more {
                    break;
                }
            }
            if !more {
                return effect;
            }
        }
    }

    /// One reading of the chain from its start with the entries in `cut` left out.
    fn pass(&self, cut: &[bool]) -> Vec<bool> {
        let mut signers: BTreeSet<&str> = BTreeSet::new();
        let mut effect = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            let held = index == 0 || (!cut[index] && signers.contains(entry.by.as_str()) && (entry.origin.is_empty() || signers.contains(entry.origin.as_str())));
            if held {
                match entry.kind.as_str() {
                    "add" | "machine" => {
                        signers.insert(entry.node.as_str());
                    }
                    "revoke" => {
                        signers.remove(entry.node.as_str());
                    }
                    _ => {}
                }
            }
            effect.push(held);
        }
        effect
    }

    /// The entries in effect, in order (`Log::in_effect`).
    fn effective(&self) -> Vec<&Entry> {
        let effect = self.in_effect();
        self.entries.iter().zip(effect).filter(|(_, held)| *held).map(|(entry, _)| entry).collect()
    }

    /// The index of the entry the removal at `index` follows: the one `after` names (an entry's
    /// hash is the `prev` of the entry after it), or the entry before the removal when `after`
    /// is empty (a removal written before the field) or names the removal's own `prev`.
    fn followed(&self, index: usize) -> usize {
        let entry = &self.entries[index];
        let before = index.saturating_sub(1);
        if entry.after.is_empty() || entry.after == entry.prev {
            return before;
        }
        (0..before).rev().find(|at| self.entries[at + 1].prev == entry.after).unwrap_or(before)
    }

    /// Whether `key` may sign on this chain as it stands: a member or a computer in effect.
    fn holds(&self, key: &str) -> bool {
        self.is_member(key) || self.machines().contains_key(key)
    }

    /// Check the chain: hashes link, every entry is signed by a member of the chain so far (the
    /// first by itself), a removal's `after` names an entry before it, and once the chain holds
    /// an entry with `after` or `origin` no later removal is without `after`; truncate to the
    /// valid prefix. Which of the valid entries are in effect is `Log::in_effect`. A signer is a
    /// member of the chain so far when every entry before counts, or when only those in effect
    /// do (a key whose removal lost its effect signs on). The prefix is checked again after a
    /// cut, since what is in effect is read from the whole chain.
    pub fn validate(&mut self) -> Result<(), String> {
        let mut first = None;
        while let Err(problem) = self.check() {
            first.get_or_insert(problem);
        }
        first.map_or(Ok(()), Err)
    }

    /// One reading for `Log::validate`: an error when it cut the chain.
    fn check(&mut self) -> Result<(), String> {
        let effect = self.in_effect();
        let mut held: BTreeSet<&str> = BTreeSet::new();
        let mut members: BTreeMap<String, ()> = BTreeMap::new();
        let mut prev = String::new();
        let mut hashes: BTreeSet<String> = BTreeSet::new();
        let mut pointed = false;
        let mut valid = 0;
        let mut problem = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.prev != prev || entry.seq != index as u64 + 1 {
                problem = Some(format!("entry {} does not follow the one before it", entry.seq));
                break;
            }
            let signer_ok = if index == 0 { entry.kind == "add" && entry.by == entry.node } else { members.contains_key(&entry.by) || held.contains(entry.by.as_str()) };
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
            if entry.kind == "revoke" && (if entry.after.is_empty() { pointed } else { !hashes.contains(&entry.after) }) {
                problem = Some(format!("entry {} is a removal that follows no entry of this chain", entry.seq));
                break;
            }
            pointed |= !entry.after.is_empty() || !entry.origin.is_empty();
            match entry.kind.as_str() {
                "add" | "machine" => {
                    members.insert(entry.node.clone(), ());
                }
                "revoke" => {
                    members.remove(&entry.node);
                }
                _ => {}
            }
            if effect[index] {
                match entry.kind.as_str() {
                    "add" | "machine" => {
                        held.insert(entry.node.as_str());
                    }
                    "revoke" => {
                        held.remove(entry.node.as_str());
                    }
                    _ => {}
                }
            }
            prev = entry.hash();
            hashes.insert(prev.clone());
            valid = index + 1;
        }
        self.entries.truncate(valid);
        match problem {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Append an entry signed by this node. A `revoke` follows the head it is appended to.
    pub fn append(&mut self, me: &Identity, kind: &str, node: &str, name: &str, addrs: Vec<String>) -> Entry {
        self.say(me, kind, node, name, addrs, "", None)
    }

    /// Append an entry signed by `me` that `origin` first said on another copy (empty: `me`
    /// says it first). A `revoke` follows `after`, or the head it is appended to.
    #[allow(clippy::too_many_arguments)]
    fn say(&mut self, me: &Identity, kind: &str, node: &str, name: &str, addrs: Vec<String>, origin: &str, after: Option<String>) -> Entry {
        let prev = self.entries.last().map(Entry::hash).unwrap_or_default();
        let after = if kind == "revoke" { after.unwrap_or_else(|| prev.clone()) } else { String::new() };
        let mut entry = Entry {
            seq: self.entries.len() as u64 + 1,
            prev,
            kind: kind.to_owned(),
            node: node.to_owned(),
            name: name.to_owned(),
            addrs,
            by: me.hex(),
            ts_ms: now_ms(),
            after,
            origin: origin.to_owned(),
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
    /// pushes it. A copy with another first entry is another network and is refused. What a key
    /// removed on either copy said without its removal following it is not carried and has no
    /// effect where it stands (`Log::replay`, `Log::in_effect`).
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
    /// `me` and naming the key that first said it (`Entry::origin`). What this chain already says
    /// is skipped, and a member this chain removed stays out, so replaying the same copy twice
    /// adds nothing the second time. A removal said again is signed by `me`, so it counts as
    /// removed even where the lost entry was a leave.
    ///
    /// Only what is in effect on `other` is said again, and only while the key that first said
    /// it may sign on this chain: what a key this chain removed (or never held) said on `other`
    /// is not carried, nor is anything said by those it admitted there. A removal said again
    /// follows what its first form followed (`Log::point`), so what the removed key said on this
    /// chain that the removal never saw loses its effect; it is said again even where the key is
    /// already out, when this chain holds such an entry. Two removals that take out each other's
    /// signer, neither following the other, both stand (`Log::took_out`). A removal said again
    /// can bring back a key an earlier entry of `other` removed (it cuts that key's removal
    /// here), so `other` is read again until a reading adds nothing. Nothing is said once `me`
    /// is no member of this chain.
    pub fn replay(&mut self, me: &Identity, other: &Log, from: usize) {
        let own = self.entries.len();
        for _ in 0..=other.entries.len() {
            let before = self.entries.len();
            self.replay_once(me, other, from, own);
            if self.entries.len() == before {
                break;
            }
        }
    }

    /// One reading of `other` for `Log::replay`; this chain's own entries end at `own`.
    fn replay_once(&mut self, me: &Identity, other: &Log, from: usize, own: usize) {
        let (their_members, their_machines) = (other.members(), other.machines());
        let in_effect = other.in_effect();
        let mine = me.hex();
        for (index, entry) in other.entries.iter().enumerate().skip(from) {
            if !self.is_member(&mine) {
                return;
            }
            if !in_effect[index] {
                continue;
            }
            let node = entry.node.as_str();
            let author = entry.author();
            let gone_there = !their_members.contains_key(node) && !their_machines.contains_key(node);
            if !self.holds(author) {
                if entry.kind == "revoke" && gone_there && node != mine && self.took_out(author, node, from) {
                    // This chain's removal of `author` was signed by `node`, and `author` removed
                    // `node` on the other copy: `node` goes with everything it said here since
                    // the copies parted (which brings `author` back), then `author` goes.
                    let after = self.point(other, index, from, own);
                    if self.holds(node) || self.said_after(node, after.as_deref()) {
                        self.say(me, "revoke", node, &entry.name, Vec::new(), "", after);
                    }
                    if self.holds(author) {
                        let named = self.members().get(author).map(|member| member.name.clone()).or_else(|| self.machines().get(author).cloned()).unwrap_or_default();
                        self.say(me, "revoke", author, &named, Vec::new(), "", None);
                    }
                }
                continue;
            }
            let origin = if author == mine { "" } else { author };
            match entry.kind.as_str() {
                "machine" => {
                    if their_machines.contains_key(node) && !self.machines().contains_key(node) && !self.removed(node) {
                        self.say(me, "machine", node, &entry.name, Vec::new(), origin, None);
                    }
                }
                "add" => {
                    if let Some(member) = their_members.get(node)
                        && !self.is_member(node)
                        && !self.removed(node)
                    {
                        self.say(me, "add", node, &member.name, member.addrs.clone(), origin, None);
                    }
                }
                "revoke" => {
                    // Said here already (this copy was read before): nothing more.
                    if gone_there && !self.took_out(node, author, from) {
                        let after = self.point(other, index, from, own);
                        if self.holds(node) || self.said_after(node, after.as_deref()) {
                            self.say(me, "revoke", node, &entry.name, Vec::new(), origin, after);
                        }
                    }
                }
                "rename" => {
                    if let (Some(theirs), Some(ours)) = (their_members.get(node), self.members().get(node))
                        && theirs.name != ours.name
                    {
                        self.say(me, "rename", node, &theirs.name, Vec::new(), origin, None);
                    }
                }
                "addrs" => {
                    if let (Some(theirs), Some(ours)) = (their_members.get(node), self.members().get(node))
                        && theirs.addrs != ours.addrs
                    {
                        self.say(me, "addrs", node, "", theirs.addrs.clone(), origin, None);
                    }
                }
                "link" => {
                    if !self.linked(node) {
                        self.say(me, "link", node, "", Vec::new(), origin, None);
                    }
                }
                _ => {}
            }
        }
    }

    /// What the removal at `index` of `other`, said again on this chain, follows here. `from`
    /// entries are shared and this chain's own entries end at `own`. The entry the removal
    /// follows on `other` when both copies hold it; else the last shared entry when this chain's
    /// own part holds anything the removed key first said (the removal never saw it); else
    /// `None`: the head at the moment it is said again, since all of the key's that came before
    /// it here was carried from `other`, where the removal followed it.
    fn point(&self, other: &Log, index: usize, from: usize, own: usize) -> Option<String> {
        let node = other.entries[index].node.as_str();
        let followed = other.followed(index);
        let at = if followed < from {
            followed
        } else if self.entries[from.min(own)..own].iter().any(|entry| entry.author() == node) {
            from.checked_sub(1)?
        } else {
            return None;
        };
        self.entries.get(at).map(Entry::hash)
    }

    /// Whether this chain holds an entry in effect that `node` first said after the entry with
    /// the hash `after` (`None`: the head, so nothing).
    fn said_after(&self, node: &str, after: Option<&str>) -> bool {
        let Some(after) = after else { return false };
        let Some(at) = self.entries.iter().position(|entry| entry.hash() == after) else { return false };
        let effect = self.in_effect();
        self.entries.iter().enumerate().skip(at + 1).any(|(index, entry)| effect[index] && entry.author() == node)
    }

    /// Whether, after its first `from` entries, this chain holds a removal of `gone` in effect
    /// that `by` first said. With a removal of `by` first said by `gone` on the other copy, the
    /// two removals take out each other's signer and neither follows the other; with the same
    /// removal on the other copy, it was said here already.
    fn took_out(&self, gone: &str, by: &str, from: usize) -> bool {
        let effect = self.in_effect();
        self.entries.iter().enumerate().skip(from).any(|(index, entry)| effect[index] && entry.kind == "revoke" && entry.node == gone && entry.author() == by)
    }

    /// The network this chain is: the hash of its first entry (`Mesh::network` shows its first ten
    /// characters).
    pub fn id(&self) -> Option<String> {
        self.entries.first().map(Entry::hash)
    }

    /// Whether this chain took in the network `id` (a `link` entry naming it).
    pub fn linked(&self, id: &str) -> bool {
        self.effective().iter().any(|entry| entry.kind == "link" && entry.node == id)
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
        let revoked: BTreeSet<String> = self.effective().iter().filter(|entry| entry.kind == "revoke").map(|entry| entry.node.clone()).collect();
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
        for entry in self.effective() {
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
        let entries = self.effective();
        let machine_keys: BTreeSet<&str> = entries.iter().filter(|entry| entry.kind == "machine").map(|entry| entry.node.as_str()).collect();
        entries
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
        let entries = self.effective();
        let brought = |key: &str, by: &str| entries.iter().any(|entry| entry.kind == "machine" && entry.node == key && entry.by == by);
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
