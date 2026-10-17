//! `cc-research`: the CortexCode `research@1` plugin. Search (arXiv, Semantic Scholar, GitHub,
//! Reddit, Hacker News, the web), fetch pages and PDFs as clean text, cited notes, and deep research
//! as a background map-reduce job with the local model. Online only when a network exists; no cloud
//! models, no paid APIs, no keys required.

pub mod config;
pub mod deep;
pub mod extract;
pub mod fetch;
pub mod net;
pub mod notes;
pub mod peers;
pub mod probe;
pub mod report;
pub mod service;
pub mod sources;
pub mod text;
