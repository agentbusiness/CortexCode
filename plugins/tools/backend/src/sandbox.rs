//! The write sandbox every agent command runs in: kept in the shared `sandbox-kit` (the same in
//! CortexCode and CortexTerm), re-exported here so the plugin names it as before.

pub use cc_sandbox_kit::sandbox::*;
