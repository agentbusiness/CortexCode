//! `mesh@1`: the mesh of this person's CortexCode instances (`docs/MESH-IMPLEMENTATION-PLAN.md` §5).
//!
//! A node is one installation with one Ed25519 key; the key is its address. A *cortex* is one
//! person's set of nodes: a signed membership log every member holds. Members connect to each other
//! (direct on a LAN, hole-punched or relayed elsewhere) and exchange capability cards. A session
//! stays at home; every member lends its model and its hands (`tools.run`, `lang-*`, `container`)
//! to every other, always through the frame's own contracts, forwarded. Nothing to configure: one
//! invitation joins a computer, Remove takes it out; what runs where is decided from the cards.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// `{}` -> [`super::MeshStatus`].
    pub const STATUS: &str = "mesh.status";
    /// `{}` -> [`super::PairCode`]: an invitation for one other computer to join with (one hour,
    /// used once).
    pub const PAIR_START: &str = "mesh.pair.start";
    /// [`super::PairJoinParams`] -> [`super::Member`]: join the cortex a code was shown on.
    pub const PAIR_JOIN: &str = "mesh.pair.join";
    /// `{}` -> `{}`: stop showing a code.
    pub const PAIR_CANCEL: &str = "mesh.pair.cancel";
    /// `{}` -> [`super::MembersResult`].
    pub const MEMBERS: &str = "mesh.members";
    /// [`super::NodeParams`] -> `{}`: a member is taken out of the network (its connections close
    /// and it finds itself alone).
    pub const REVOKE: &str = "mesh.revoke";
    /// `{}` -> `{}`: this computer leaves its network: the others drop it, it keeps nothing of
    /// the network and can be invited into another.
    pub const LEAVE: &str = "mesh.leave";
    /// `{}` -> [`super::Card`]: this node's capability card.
    pub const CARD: &str = "mesh.card";
    /// `{}` -> [`super::PeersResult`]: every member's card and reachability.
    pub const PEERS: &str = "mesh.peers";
    /// [`super::CallParams`] -> the called method's result (stream items pass through): one call on
    /// a member's frame (what members lend: model and hands methods).
    pub const CALL: &str = "mesh.call";
    /// [`super::NameParams`] -> `{}`: this node's name as the others see it.
    pub const NAME: &str = "mesh.name";
    /// [`super::RunnerParams`] -> [`super::Runner`]: which member, if any, runs a session's
    /// commands (the tools plugin asks once per session; the answer carries the member's copy of
    /// the tree, made and kept in step by the mesh). `node` absent: the commands run here.
    pub const RUNNER: &str = "mesh.runner";
    /// [`super::TreeFilesParams`]: this node's tree as items `{path, mode, data}` (base64), the
    /// files git tracks or would track; a member copying the tree asks for it.
    pub const TREE_FILES: &str = "mesh.tree.files";
    /// [`super::TreeApplyParams`] -> `{}`: files to write into a copy of a tree kept here, or
    /// into the tree itself when a member's command changed them.
    pub const TREE_APPLY: &str = "mesh.tree.apply";
    /// [`super::PlaceParams`] -> [`super::Placement`]: a session's model calls go to a joined
    /// computer with a free model of the same family, when there is one; else they stay here.
    /// Automatic: the agents plugin asks for every agent it starts, the chat flow for a session
    /// whose turn starts while this computer's model is busy.
    pub const PLACE: &str = "mesh.place";
    /// [`super::PlaceParams`] -> `{}`: the session's model calls stay here again (its run ended).
    pub const PLACE_END: &str = "mesh.place.end";
    /// `{}` -> [`super::Capacity`]: how many joined computers could take a session's model calls
    /// now (a free model of this computer's family). The chat flow tells its planner the number,
    /// nothing more.
    pub const CAPACITY: &str = "mesh.capacity";
}

pub mod topics {
    /// [`super::Changed`]: members, cards, reachability or served categories changed.
    pub const CHANGED: &str = "mesh.changed";
}

/// What a member may do for this cortex. Deny by default: a member with no grant can only be seen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Member {
    /// The node's public key, hex.
    pub node: String,
    pub name: String,
    pub added_ms: u64,
    /// The member that added this one (hex), `self` for the first.
    #[serde(default)]
    pub by: String,
    /// The last addresses the member was reached at (a relay URL, direct addresses), to dial it
    /// without a lookup.
    #[serde(default)]
    pub addrs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MembersResult {
    pub members: Vec<Member>,
}

/// The facts a node tells the others about itself: what its plugins already report, nothing typed
/// by hand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Card {
    pub node: String,
    pub name: String,
    /// `cortexcode` (or another app on the mesh), and its version.
    pub software: String,
    pub version: String,
    pub chip: String,
    pub memory_bytes: u64,
    pub cpu_cores: u32,
    pub gpu_cores: u32,
    pub os_version: String,
    /// Free bytes on the home folder's volume.
    #[serde(default)]
    pub free_disk_bytes: u64,
    #[serde(default)]
    pub free_memory_bytes: u64,
    #[serde(default)]
    pub on_mains: bool,
    /// A person is working in the app here (a foreground session ran recently).
    #[serde(default)]
    pub user_active: bool,
    /// The model plugin serving `model`, its model and variant, context, free slots and speed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelCard>,
    /// Every plugin, with the hash of its live data (prompts, skills, texts), so a lender's plugin
    /// is known to behave exactly as the borrower's.
    #[serde(default)]
    pub plugins: Vec<PluginCard>,
    /// Language plugins present, with the version of the language's toolchain when known.
    #[serde(default)]
    pub languages: Vec<LanguageCard>,
    #[serde(default)]
    pub docker: bool,
    /// Sessions running here now, and agents.
    #[serde(default)]
    pub sessions_running: u32,
    #[serde(default)]
    pub agents_running: u32,
    /// The agents scheduler's measured limit.
    #[serde(default)]
    pub agent_limit: u32,
    /// Projects known here by repository identity (the root commit's hash), so work lands where
    /// its code already is.
    #[serde(default)]
    pub repositories: Vec<String>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelCard {
    pub plugin: String,
    pub model_id: String,
    /// `medium` | `small` | the engine's own variant name.
    pub variant: String,
    pub family: String,
    pub ctx_total: u32,
    pub slots: u32,
    pub slots_busy: u32,
    /// Slots running a request a person waits for.
    #[serde(default)]
    pub busy_foreground: u32,
    pub queued: u32,
    /// Measured decode speed, tokens per second (0 when not measured yet).
    #[serde(default)]
    pub tok_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginCard {
    pub id: String,
    pub category: String,
    pub version: String,
    /// SHA-256 over the plugin's live data folder (empty when it has none).
    #[serde(default)]
    pub data_hash: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LanguageCard {
    pub id: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Peer {
    pub member: Member,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    pub reachable: bool,
    /// `lan` (direct, a private address) | `direct` | `relay` | `` (not connected).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub rtt_ms: u32,
    /// Categories this node borrows from the member now.
    #[serde(default)]
    pub serving: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PeersResult {
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MeshStatus {
    pub node: String,
    pub name: String,
    /// The addresses this node can be reached at now.
    #[serde(default)]
    pub addrs: Vec<String>,
    pub members: u32,
    pub reachable: u32,
    /// The network this computer is on (absent: on none, or alone in one it started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    /// An invitation is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairCode>,
    /// Categories borrowed from members as a whole (this computer has no model of its own):
    /// category -> node.
    #[serde(default)]
    pub serving: Vec<(String, String)>,
    /// Sessions whose model calls run on a member now.
    #[serde(default)]
    pub placed: Vec<Placed>,
}

/// A network: the computers joined together, named by the computer that started it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Network {
    /// Short id (the hash of the first membership entry).
    pub id: String,
    pub founder: String,
    pub founded_ms: u64,
    pub members: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Placed {
    pub session_id: String,
    pub node: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PairCode {
    /// Four words, typed on the other computer.
    pub code: String,
    pub expires_ms: u64,
    /// On another network the code alone cannot find this node: the full invitation carries the
    /// node's addresses too (`<code>@<ticket>`).
    #[serde(default)]
    pub invitation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PairJoinParams {
    /// The four words, or the full invitation.
    pub code: String,
    /// This node's name as the other side will see it (defaults to the computer's name).
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NodeParams {
    pub node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NameParams {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CallParams {
    pub node: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Payload of [`topics::CHANGED`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Changed {
    /// `members` | `cards` | `peers` | `serving` | `placed` | `pairing`
    pub what: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunnerParams {
    pub workspace: String,
    pub session_id: String,
    /// The project's id at home (what a member names the copy by).
    #[serde(default)]
    pub project_id: String,
}

/// Where a session's commands run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Runner {
    /// The member (hex); absent: here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub name: String,
    /// The member's copy of the tree (its absolute path there), ready to run in.
    #[serde(default)]
    pub remote_workspace: String,
    /// Why no member runs them, when none does (for the person, in the card).
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TreeFilesParams {
    pub workspace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TreeFile {
    pub path: String,
    #[serde(default)]
    pub mode: u32,
    /// Base64 of the content; absent: the file is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TreeApplyParams {
    /// The tree at the receiver: a copy's key (`copy:<home project id>`) or a workspace path.
    pub target: String,
    pub files: Vec<TreeFile>,
    /// Who made the change, for the receiver's step log (`session:<id>`, `node:<id>`).
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaceParams {
    pub session_id: String,
    /// Wait until a model is free (a person's turn); false: answer at once (an agent takes what
    /// is free now, else it stays queued here).
    #[serde(default)]
    pub wait: bool,
    /// Only a member's model counts (this computer's agent queue is full).
    #[serde(default)]
    pub remote_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Capacity {
    /// Members with a free model of this computer's family, reachable now.
    pub free: u32,
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Placement {
    /// The member whose model serves the session (hex); absent: this computer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model_id: String,
    /// `local` | `lan` | `remote`.
    #[serde(default)]
    pub tier: String,
}
