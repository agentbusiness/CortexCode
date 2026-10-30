//! `cc-vcs`: the CortexCode `vcs@1` plugin, built on the git command-line tool.

pub mod checkpoint;
pub mod clone;
pub mod diff;
pub mod git;
pub mod github;
pub mod graph;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "linux")]
mod landlock;
pub mod ops;
pub mod probe;
pub mod remote;
pub mod repo;
pub mod service;
pub mod status;
pub mod watch;
