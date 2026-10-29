//! `mesh@1`: the mesh of this person's CortexCode instances (`docs/MESH-IMPLEMENTATION-PLAN.md` §5).
//!
//! A node is one installation with one Ed25519 key; the key is its address. A *cortex* is one
//! person's set of nodes: a signed membership log every member holds. Members connect to each other
//! (direct on a LAN, hole-punched or relayed elsewhere) and exchange capability cards. A project
//! lives on its own computer, and every member's window can work in it there (`mesh.link`). Every
//! member lends its model to every other unless its person turns that off (`mesh.lend`), through
//! the frame's own `model` contract, forwarded; a
//! request names the work it is for (a tag, `coding`), and only a model tagged for that work takes
//! it. One invitation joins a computer, Remove takes it out; what runs where
//! is decided from the cards.

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
    /// a member's frame (what members lend: its model's methods).
    pub const CALL: &str = "mesh.call";
    /// [`super::NameParams`] -> `{}`: this node's name as the others see it.
    pub const NAME: &str = "mesh.name";
    /// [`super::PlaceParams`] -> [`super::Placement`]: a session's model calls go to a joined
    /// computer with a free model of the same family and tag, when there is one; else they stay
    /// here.
    /// Automatic: the agents plugin asks for every agent it starts, the chat flow for a session
    /// whose turn starts while this computer's model is busy.
    pub const PLACE: &str = "mesh.place";
    /// [`super::PlaceParams`] -> `{}`: the session's model calls stay here again (its run ended).
    pub const PLACE_END: &str = "mesh.place.end";
    /// [`super::CapacityParams`] -> [`super::Capacity`]: how many joined computers could take a
    /// session's model calls now (a free model of this computer's family and the tag). The chat
    /// flow tells its planner the number, nothing more.
    pub const CAPACITY: &str = "mesh.capacity";
    /// `{}` -> [`super::ProjectsResult`]: every project of the network, this computer's and every
    /// member's, with its description and computer. A project is worked on at its own computer.
    pub const PROJECTS: &str = "mesh.projects";
    /// [`super::NodeParams`] -> [`super::Link`]: a local address that leads to a member's frame
    /// (each connection to it is carried over the mesh), and the token that frame takes. The
    /// window talks to the computer of the project it shows through it.
    pub const LINK: &str = "mesh.link";
    /// [`super::ClaimParams`] -> [`super::Placement`]: a resource of the network for one use by a
    /// session (`kind: model`: one model call, on a model tagged for the request's `tag`). The best
    /// free one is taken; when none is free the
    /// claim waits, and a freed resource goes to the oldest waiting run (a run's age is its first
    /// claim, kept until `mesh.place.end`). The frame then routes the session's calls there.
    pub const CLAIM: &str = "mesh.claim";
    /// [`super::ClaimParams`] -> `{}`: the use is over; the resource is free for the next run.
    pub const RELEASE: &str = "mesh.release";
    /// [`super::LendParams`] -> [`super::MeshStatus`]: whether members may use this computer's
    /// model (off: its card shows no model and its claims are refused); this computer still uses
    /// theirs.
    pub const LEND: &str = "mesh.lend";
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
    /// The projects open here, worked on here from any computer.
    #[serde(default)]
    pub projects: Vec<ProjectCard>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProjectCard {
    pub id: String,
    pub name: String,
    /// The project's description from its memory (empty until it is written).
    #[serde(default)]
    pub description: String,
    pub created_ms: u64,
}

/// A project of the network and the computer it lives on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NetworkProject {
    #[serde(flatten)]
    pub project: ProjectCard,
    /// The computer (hex); empty: this one.
    pub node: String,
    pub computer: String,
    /// The computer is connected now (always true for this one's).
    pub reachable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProjectsResult {
    pub projects: Vec<NetworkProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Link {
    /// `http://127.0.0.1:<port>`: the member's frame, reached over the mesh.
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelCard {
    pub plugin: String,
    pub model_id: String,
    /// `medium` | `small` | the engine's own variant name.
    pub variant: String,
    pub family: String,
    /// The work the model is for ([`crate::contracts::model::tags`]).
    #[serde(default = "crate::contracts::model::default_tags")]
    pub tags: Vec<String>,
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
    /// Members may use this computer's model (`mesh.lend`).
    #[serde(default = "lends_by_default")]
    pub lend: bool,
}

fn lends_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LendParams {
    pub lend: bool,
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
pub struct PlaceParams {
    pub session_id: String,
    /// The work the session is for: only a model with this tag takes it.
    #[serde(default = "crate::contracts::model::default_tag")]
    pub tag: String,
    /// Wait until a model is free (a person's turn); false: answer at once (an agent takes what
    /// is free now, else it stays queued here).
    #[serde(default)]
    pub wait: bool,
    /// Only a member's model counts (this computer's agent queue is full).
    #[serde(default)]
    pub remote_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ClaimParams {
    /// The resource's kind: `model`.
    pub kind: String,
    pub session_id: String,
    /// The work the call is for: only a model with this tag takes it.
    #[serde(default = "crate::contracts::model::default_tag")]
    pub tag: String,
    /// A person waits for this use (a chat turn); on its own computer it does not wait for agent
    /// work, which the engine puts behind it.
    #[serde(default)]
    pub foreground: bool,
    /// Wait until a model is free (the default). false: only a model free now (a prompt warm-up
    /// that is pointless on a busy model); the answer's `tier` is `busy` when none is.
    #[serde(default = "claim_waits")]
    pub wait: bool,
    /// The prompt's size in tokens: only a model whose conversation holds it takes the call (0:
    /// unknown, any model).
    #[serde(default)]
    pub tokens: u32,
}

fn claim_waits() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CapacityParams {
    /// The work the sessions would be for: only models with this tag count.
    #[serde(default = "crate::contracts::model::default_tag")]
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Capacity {
    /// Members with a free model of this computer's family and the tag, reachable now.
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
