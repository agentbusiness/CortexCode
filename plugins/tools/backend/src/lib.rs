//! CortexCode tools plugin (`tools@1`): the model's read / edit / run tools with hashline anchors,
//! argument repair, URI reads, a persistent shell with background jobs, output offloading,
//! copy-on-write workspaces and the UI file tree.

pub mod approval;
pub mod artifacts;
pub mod baseline;
pub mod cleanup;
pub mod codeskills;
pub mod core;
pub mod doc;
pub mod exec;
pub mod guard;
pub mod hashedit;
pub mod host;
pub mod jobs;
pub mod lang;
#[cfg(target_os = "linux")]
pub mod landlock;
pub mod langs;
pub mod pyenv;
pub mod read;
pub mod readstate;
pub mod repair;
pub mod research;
pub mod sandbox;
pub mod schemas;
pub mod service;
pub mod shell;
pub mod term;
pub mod testout;
pub mod textedit;
pub mod tree;
pub mod undo;
pub mod uris;
pub mod util;
pub mod workspace;
pub mod xproject;
