//! The write sandbox every agent command runs in: macOS `sandbox-exec`, Linux Landlock.
//!
//! Part of the shared standard of CortexCode and CortexTerm: this folder is the same, byte for byte,
//! in both repositories. A change is made in both, in the same piece of work.
//!
//! - [`sandbox`]: the policy ([`sandbox::Policy`]), a session's confinement ([`sandbox::Sandbox`]),
//!   the approval classes that widen it, and how a refused write reads in command output;
//! - [`landlock`]: the Linux ruleset behind [`sandbox::Sandbox`] there.

#[cfg(target_os = "linux")]
pub mod landlock;
pub mod sandbox;
mod util;
