//! Apps of this person on this machine: members without pairing.
//!
//! Every running frame's mesh writes one small file into a folder only this macOS user can open
//! (`~/.cortex-mesh/local/<software>-<node>.json`, the folder 0700, the file 0600; `CORTEX_MESH_LOCAL_DIR`
//! moves it): its node id, software, frame home, process id and the loopback addresses its
//! endpoint listens on. The others read it, skip files whose process is gone, and take those
//! nodes as members marked `local`, dialed straight over loopback (no relay, no lookup).
//!
//! The trust lives only here, never in the signed membership log: each app keeps its own log (and
//! may be in a network with other computers), a log entry would admit the node on every member
//! computer too, and logs of two networks cannot merge. Being able to write into this user's
//! folder is the proof; the trust ends when the file or its process goes.

use cc_protocol::contracts::mesh::Member;
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalPeer {
    pub node: String,
    pub software: String,
    pub name: String,
    /// The frame's home folder.
    pub home: String,
    pub pid: u32,
    /// Loopback addresses of its endpoint (`127.0.0.1:<port>`).
    pub addrs: Vec<String>,
}

impl LocalPeer {
    /// As a member: the address to dial is the loopback one.
    pub fn member(&self) -> Option<Member> {
        let id = self.node.parse::<EndpointId>().ok()?;
        let addr = EndpointAddr::from_parts(id, self.addrs.iter().filter_map(|addr| addr.parse().ok()).map(TransportAddr::Ip));
        Some(Member { node: self.node.clone(), name: format!("{} ({})", self.name, self.software), added_ms: 0, by: String::new(), addrs: vec![serde_json::to_string(&addr).ok()?], local: true })
    }
}

pub fn dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CORTEX_MESH_LOCAL_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".cortex-mesh").join("local"))
}

fn file_of(software: &str, node: &str) -> Option<PathBuf> {
    Some(dir()?.join(format!("{software}-{}.json", &node[..node.len().min(16)])))
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid != 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

/// Write (or refresh) this node's file.
pub fn announce(me: &LocalPeer) {
    let (Some(dir), Some(file)) = (dir(), file_of(&me.software, &me.node)) else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let Ok(text) = serde_json::to_vec_pretty(me) else { return };
    if std::fs::read(&file).is_ok_and(|current| current == text) {
        return;
    }
    let tmp = file.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::rename(&tmp, &file);
    }
}

/// Remove this node's file (on shutdown), unless a newer process of it wrote it since.
pub fn withdraw(software: &str, node: &str) {
    let Some(file) = file_of(software, node) else { return };
    let owner = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<LocalPeer>(&bytes).ok()).map(|peer| peer.pid);
    if owner == Some(std::process::id()) {
        let _ = std::fs::remove_file(file);
    }
}

/// The other apps running on this machine as this user: node -> peer.
pub fn discover(me: &str) -> HashMap<String, LocalPeer> {
    let mut found = HashMap::new();
    let Some(dir) = dir() else { return found };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let Ok(meta) = std::fs::metadata(&dir) else { return found };
    // Only a folder of this user that nobody else can write into is a proof.
    if meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return found;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return found };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.uid() != uid || meta.mode() & 0o022 != 0 {
            continue;
        }
        let Some(peer) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<LocalPeer>(&bytes).ok()) else { continue };
        if peer.node == me || !alive(peer.pid) {
            continue;
        }
        found.insert(peer.node.clone(), peer);
    }
    found
}

/// Loopback addresses for the sockets an endpoint is bound to.
pub fn loopback(bound: &[std::net::SocketAddr]) -> Vec<String> {
    bound
        .iter()
        .map(|addr| match addr {
            std::net::SocketAddr::V4(v4) => std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, v4.port())),
            std::net::SocketAddr::V6(v6) => std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, v6.port())),
        })
        .map(|addr| addr.to_string())
        .collect()
}

impl crate::service::Mesh {
    /// One model per machine, asked by this frame's model plugin before it loads a model tagged
    /// `to_tag`: every app on this machine whose model is not tagged so unloads it. Each is
    /// reached directly (dialed now when not connected yet: the apps may have started together)
    /// and its card is read live; the answers come when they have unloaded.
    pub async fn ask_local_yield(self: &std::sync::Arc<Self>, to_tag: &str) -> cc_protocol::contracts::mesh::YieldResult {
        self.refresh_local();
        let locals: Vec<Member> = self.members().into_values().filter(|member| member.local).collect();
        let asks = locals.into_iter().map(|member| {
            let mesh = self.clone();
            let to_tag = to_tag.to_owned();
            async move {
                if mesh.connection(&member.node).is_none() && !mesh.dial(&member).await {
                    return None;
                }
                let card = mesh.call_member(&member.node, "mesh.card", serde_json::json!({}), None, tokio_util::sync::CancellationToken::new()).await.ok()?;
                let model: cc_protocol::contracts::mesh::ModelCard = serde_json::from_value(card.get("model")?.clone()).ok()?;
                if model.tags.contains(&to_tag) {
                    return None;
                }
                tracing::info!(app = %member.name, model = %model.model_id, to_tag, "asking an app on this machine to unload its model");
                let answer = mesh.call_member(&member.node, cc_protocol::contracts::mesh::methods::MODEL_YIELD, serde_json::json!({"to_tag": to_tag}), None, tokio_util::sync::CancellationToken::new()).await;
                let yielded = answer.ok().and_then(|value| value.get("yielded").and_then(serde_json::Value::as_bool)).unwrap_or(false);
                Some((member.name, yielded))
            }
        });
        let mut result = cc_protocol::contracts::mesh::YieldResult::default();
        for (name, yielded) in futures_util::future::join_all(asks).await.into_iter().flatten() {
            if yielded {
                result.yielded.push(name.clone());
            }
            result.asked.push(name);
        }
        result
    }
}
