//! CortexCode model plugin (`model@1`) on PrismML's llama.cpp, embedded in-process: Ternary
//! Bonsai 2 27B for Macs with 24 GB of memory or less.
//!
//! - [`llama`]: the vendored engine through the shim's flat C ABI.
//! - [`engine`]: the engine thread: the loaded model, its fitted context, generation.
//! - [`fit`]: the memory plan from the GGUF header and the machine.
//! - [`sampler`], [`grammar`], [`constraint`]: sampling on the host, llguidance constraints.
//! - [`runtime`]: installing and loading variants, and the `model.*` request paths.
//! - [`template`], [`parse`], [`repeat`], [`progress`], [`metrics`], [`variants`], [`machine`]:
//!   shared with the uzu plugin.
// `cc_protocol::RpcError` (128 bytes) is the protocol's error type for every contract method.
#![allow(clippy::result_large_err)]
pub mod constraint;
pub mod engine;
pub mod fit;
pub mod grammar;
pub mod llama;
pub mod machine;
pub mod metrics;
pub mod parse;
pub mod plugin;
pub mod progress;
pub mod repeat;
pub mod runtime;
pub mod sampler;
pub mod template;
pub mod variants;
