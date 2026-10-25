//! Cortex Plugin Protocol (CPP/1).
//!
//! The frame and every plugin speak this protocol over a Unix socket (plugins) or WebSocket (UI):
//! length-prefixed JSON messages ([`envelope::Message`]) carrying requests, streamed items, final
//! responses, events and cancellations. Category contracts live in [`contracts`]; the shared
//! hashline anchor definition lives in [`hashline`].

pub mod checks;
pub mod codec;
pub mod conn;
pub mod contracts;
pub mod envelope;
pub mod hashline;
pub mod host;
pub mod plugin;
pub mod sha256;
pub mod stored;

pub use conn::{BoxFuture, CallStream, Connection, Handler, IncomingRequest, ItemSink, NoHandler, Responder};
pub use envelope::{Message, RpcError, codes, parse_params, to_value};
pub use plugin::{Call, Ctx, Plugin, PluginEnv, run_plugin};

/// Protocol version spoken by this crate.
pub const PROTOCOL_VERSION: &str = "CPP/1";
