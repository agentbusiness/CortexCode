//! The shared kit of CortexCode's language plugins (the `lang` family, contract `lang@1`).
//!
//! A language plugin is one [`LanguagePack`]: its languages and their extractor (the code graph's
//! facts of a file), its language server, verifier commands and skills. [`run_pack`] serves the
//! pack over CPP/1 on the category `lang-<id>`; everything the contract needs beyond the pack's own
//! knowledge lives here:
//!
//! - [`lsp`]: a language-server client (JSON-RPC over stdio with Content-Length framing), one server
//!   per workspace root started on demand, documents synchronised from disk on every request,
//!   diagnostics collected until the server has answered and gone quiet, crash restart and idle stop;
//! - [`install`]: toolchain installation into `$CORTEX_HOME/toolchains/<name>/` (npm packages,
//!   checksummed release archives) and discovery of executables already present;
//! - [`skills`]: the plugin's `data/skills/*.md`, the diagnostic codes `data/skills/codes.toml`
//!   declares for them and the projects `data/skills/applies.toml` says they apply to;
//! - [`skills_plugin`]: a whole skills plugin (the `skills` family, contract `skills@1`), served by
//!   [`run_skills_plugin`] from a skills directory;
//! - [`syntax`]: tree-sitter helpers shared by the extractors, and [`manifest`] helpers shared by
//!   manifest extractors.

pub mod install;
pub mod lsp;
pub mod manifest;
pub mod pack;
pub mod skills;
pub mod skills_plugin;
pub mod syntax;

pub use cc_protocol;
pub use install::FoundServer;
pub use pack::{ExtractContext, LangPlugin, LanguagePack, PackEnv, Sample, language_of, run_pack};
pub use skills_plugin::{SkillsPlugin, run_skills_plugin};
pub use tree_sitter;
