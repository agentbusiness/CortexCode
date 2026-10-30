# Mesh (`mesh`)

This person's CortexCode instances joined together (`docs/MESH-IMPLEMENTATION-PLAN.md`). A node is
one installation with one Ed25519 key (in the Keychain: `CortexCode mesh identity`, account = the home); the key is its id and its address
(iroh: QUIC with TLS 1.3 end to end, direct on the local network, hole-punched or relayed through
n0's public relays elsewhere; a relay carries only encrypted traffic and holds nothing). A
*cortex* is one person's set of nodes: a signed, append-only membership log every member holds
(stream `mesh.members` in the app's graph); a connection from a key the log does not hold is refused, and no
server can add a node.

## What it keeps (`src/store.rs`)

Everything in the graph the memory plugin serves (`docs/GRAPH-MODEL.md`, Mesh); no files.

- stream `mesh.members`: the membership log's entries in order (`{"reset": true}` starts a new
  chain when the node leaves or adopts another network); kept 3650 days.
- `(:Member {key: node id, name*, added_ms, added_by, software})-[:IN]->(:Mesh {key: hash of the
  first entry, founder, founded_ms})-[:ON]->(:Machine)` (WORKSPACE layer), built from the log: a
  revoked member is retired (`revoked`), a mesh left behind is retired (`left`).
- stream `mesh.served`: every call served to another app, `{at, node, method, ok, ms}`, 30 days.
- settings `mesh.name`, `mesh.lend`, `mesh.allowed` (the members that may work here, `mesh.allow`). Shared services have no switches: always on (`read` from any member, `edit` and `run` from apps on this machine).
- **Start order**: the endpoint starts at once, so apps on this Mac link and lend before the
  memory plugin answers; members of the network are let in, and the membership changed, only once
  the log is read (methods that change it wait 10 s, then answer `unavailable`). The store is
  retried every second; writes queue in order in memory until it takes them.
- Files of earlier versions (`mesh/members.jsonl`, `served.jsonl`, `shares.json`, `name`,
  `no-lend`) are moved in once, confirmed and deleted.
- `~/.cortex-mesh/local/<software>-<node>.json` stays a file: plumbing (pid and loopback ports of
  a running app, removed when it stops), how CortexCode and CortexTerm on one Mac find each other.

Nothing to set. One invitation joins a computer; Remove takes it out. Apps on one Mac join each
other's network by themselves: one in no network is taken in by one that is in a network with
other computers (`mesh.local.join`), unless it left that network or was removed from it before. Each member row has one switch: whether
that member may see its projects. A member not switched on gets no projects in this computer's card and its
window is refused. A project lives on its own
computer, and every member's window works in it there; every member lends every other its model,
for the work the model is tagged for. What runs where is decided by the host from what each computer
reports, never by the model; the model is told one number when it plans (how many other computers
can run tasks at the same time), nothing else.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, the UI package and its slot, source location |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-mesh`: category `mesh`, contract `mesh@1` |
| `ui/` | one npm workspace: `ui/settings-mesh` (settings.mesh@1) |

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build          # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/mesh --install  # packs and installs; the frame hot-swaps backend and UI together
```

## How it works

- **Joining** (`mesh.pair.start`, `mesh.pair.join`): one computer makes an invitation (good for
  one hour, for one computer); the other pastes it. The invitation is four words and this node's
  address; both sides run SPAKE2 over the words (`pairing.rs`), so the invitation becomes a strong
  key and a wrong one leaves the two with different keys, which the confirmation step shows (three
  wrong tries close the invitation). On the local network the words alone find the inviter (mDNS);
  the address in the invitation finds it anywhere else. The joiner sends its signed record and gets
  the membership log with itself added.
- **Membership** (`members.rs`): entries `add | rename | revoke | addrs`, each signed by a member
  and chained by hash; replicated on every connection (`mesh.log.push`); a longer valid chain on
  the same prefix wins. `mesh.revoke` (Remove) and `mesh.name` append entries. The network is
  the chain: its id is the first entry's hash, its name the computer that started it
  (`MeshStatus.network`). A computer is on one network at a time: `mesh.leave` revokes itself,
  tells the others and forgets the log (connections close, nothing borrowed or placed stays);
  joining another network leaves the current one first; a computer another member removed finds
  itself alone the moment the log reaches it.
- **Connections** (`net.rs`): one QUIC connection per member, dialed every 15 s while absent;
  the selected path is `lan` (direct, a private address), `direct` (hole-punched across networks)
  or `relay`. On `cortex/frame/1` every request is one bidirectional stream carrying the frame's
  own envelopes: one `Req`, then `Item`s and one `Res`; a `Cancel` or a closed stream cancels the
  forwarded call. The serving side forwards only what members lend (`lendable`: `model.*`, and `system.trace.*` and `system.traces`, which only observe a process) to
  its own frame, nothing else.
- **Cards** (`card.rs`): what the frame and the plugins report (machine, free disk and memory,
  mains, a person active, the model with its variant, family, tags, busy slots and speed, every plugin
  with the hash of its live data, languages, Docker, sessions and agents running, repositories by
  root commit), pushed to every member on connect and every 15 s.
- **Tags**: a model plugin declares the work its model is for (`ModelInfo.tags`, today
  `coding`); every request names one (`mesh.claim`, `mesh.place`, `mesh.capacity` take `tag`,
  `coding` when absent), and only a model with that tag takes it, here or on a member. A coding
  call never lands on a model that is there for other work, and the reverse.
  A claim whose tag no model has yet waits for one (one wait per tag, in age order): it looks
  again whenever the models change (a model plugin here starts or swaps, a member connects, goes
  or sends its card); a claim that does not wait hears `busy`.
- **Where a session's model runs** (`place.rs`, `mesh.place {session_id, tag, wait, remote_only}`):
  the host's decision, once per run; the frame then routes that session's model calls (their
  `cache_key` is the session id) to the chosen computer until `mesh.place.end`. The order is
  fixed: this computer first, then the local network, then remote; within a tier the higher
  variant wins (medium over small), then the faster one; only a model of the same family as this
  computer's, tagged for the session's work, counts. A model is taken only when free: running nothing (an agent never slows
  another's tokens down) or, for a person's turn, nothing a person waits for (the engine gives a
  person's request every step over agent work). A person's turn waits for the first model to free
  up (the chat shows "Waiting for a free model"); an agent takes what is free now or stays queued
  here, and an agent that would wait for this computer's model starts on a free member's instead
  (the agents plugin asks with `remote_only`). The routed client (`borrow.rs`, hello scope
  `sessions`) is one per member, opened on first use; this node's own model keeps serving
  everything else. A lender that goes away drops its routes: the session's next call runs here.
- **`mesh.capacity`**: how many members could take a session's model calls now. The chat flow
  gives its planner that number in one sentence, so it makes the tasks that can run side by side
  separate and names what must wait for what (`depends_on`).
- **A node without a model** (`service.rs` `auto_place`): borrows a reachable member's whole
  model category (the client takes the category over) and stops when that member goes away.
- `mesh.call {node, method, params}` forwards one lendable call to a member, with stream items
  passed through.
- Relays: n0's public ones by default; a company's own go in the plugin's config (`relays`).

## Settings > Mesh

This computer's name and the network it is on (who started it, when), with Leave; Make / Copy
the invitation; Join (paste an invitation); the joined computers: where they are (this network,
remote), what they have (model and its tags, free or busy, languages, Docker), what they do for this computer
right now, and Remove.
