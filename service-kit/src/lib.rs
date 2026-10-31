//! The caller's side of shared services and the text a model reads from their answers. Shared by
//! CortexCode and CortexTerm, byte for byte (README.md).

pub mod client;
pub mod text;

pub use client::{Link, Provider, Ran};
