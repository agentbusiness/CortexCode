//! CortexCode model plugin (`model@1`) on Mirai's uzu engine, embedded in-process.
//!
//! - [`engine`]: the uzu worker thread: per-conversation states, scheduling, hibernation.
//! - [`runtime`]: installing, fitting and loading variants, and every `model.*` request path.
//! - [`template`], [`parse`], [`constraint`]: prompts in, structured output out, constraints.
//! - [`endpoint`]: the local OpenAI-compatible HTTP endpoint on the same engine.
// `cc_protocol::RpcError` (128 bytes) is the protocol's error type for every contract method; the
// toolchain this plugin needs (1.99) lints large `Err` variants, the repository's (1.94) does not.
#![allow(clippy::result_large_err)]
pub mod constraint;
pub mod endpoint;
pub mod engine;
pub mod fit;
pub mod machine;
pub mod metrics;
pub mod parse;
pub mod plugin;
pub mod progress;
pub mod repeat;
pub mod runtime;
pub mod template;
pub mod variants;
pub mod vision;
