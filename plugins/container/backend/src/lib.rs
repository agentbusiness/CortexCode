//! `cc-container`: the CortexCode `container@1` plugin. A project's commands run inside a Docker
//! container while its files stay on the Mac, bind-mounted at the container's workdir.

pub mod compose;
pub mod config;
pub mod devcli;
pub mod docker;
pub mod exec;
pub mod jsonc;
pub mod paths;
pub mod probe;
pub mod service;
