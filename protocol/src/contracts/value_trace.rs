//! On-demand value-origin analysis shared by language and memory consumers.

use serde::{Deserialize, Serialize};

pub fn trace_limit() -> u32 {
    40
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ValueInput {
    pub name: String,
    /// Possible defining steps, not one arbitrarily chosen branch.
    pub definitions: Vec<u32>,
    /// An input may also come from outside the supported local analysis.
    pub unresolved: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ValueStep {
    pub id: u32,
    pub line: u32,
    pub end_line: u32,
    pub text: String,
    pub defines: Vec<String>,
    pub inputs: Vec<ValueInput>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ValueTrace {
    pub path: String,
    pub content_hash: String,
    pub function: String,
    pub line: u32,
    pub name: String,
    pub roots: Vec<u32>,
    pub steps: Vec<ValueStep>,
    /// Always supplied: assumptions, unsupported effects and incomplete paths.
    pub boundaries: Vec<String>,
    pub truncated: bool,
}
