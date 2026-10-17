//! A language-server client for the `lang@1` queries: framing ([`framing`]), positions and URIs
//! ([`position`]), the JSON-RPC client of one server ([`client`]) and servers per workspace root
//! with document synchronisation and diagnostics settling ([`servers`]).

pub mod client;
pub mod framing;
pub mod position;
pub mod servers;

pub use servers::{DiagnosticsHook, EventSink, Launch, Launched, Launcher, ServerEvent, ServerSpec, Servers, spawn_process};
