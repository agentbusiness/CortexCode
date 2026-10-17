//! `skills@1`: plugins that carry know-how for the model as skills, independent of any language
//! (`skills-engineering`: planning, design, debugging, review ...; the `skills` family, see
//! [`super::FAMILIES`]). A skill is a markdown text with a one-line summary saying when it helps;
//! the model sees the summaries in its system prompt and reads a skill with
//! `read skill://<category>/<name>`. Methods are called on the plugin's own category:
//! `skills-engineering.describe`, `skills-engineering.skill`. Language plugins (`lang@1`) serve the
//! same two methods for their own skills, and the memory plugin (`memory@1`) lists its own with
//! `memory.skills` and serves them with `memory.skill` ([`CARRIERS`]). A skill may declare the
//! exact diagnostic codes it covers ([`SkillInfo::codes`]): the tools attach it to a result that
//! shows one of them. A skill may
//! declare the projects it applies to ([`SkillInfo::applies`]): a project that shows none of them is
//! not offered it.

use serde::{Deserialize, Serialize};

pub use super::lang::{Applies, SkillInfo, SkillParams, SkillResult};

pub mod methods {
    /// The plugin's skills -> [`super::Description`].
    pub const DESCRIBE: &str = "describe";
    /// One skill's text ([`super::SkillParams`] -> [`super::SkillResult`]).
    pub const SKILL: &str = "skill";
    pub const ALL: &[&str] = &[DESCRIBE, SKILL];
}

pub mod topics {
    /// `{category}`: published by a plugin that serves skills (any carrier) when the skills in use
    /// changed: their names, summaries, codes or conditions are to be asked again.
    pub const CHANGED: &str = "skills.changed";
}

/// The contract name.
pub const CONTRACT: &str = "skills@1";

/// Contracts whose plugins carry skills, with the method that lists them (called as
/// `<category>.<method>`, answering with a `skills` array of [`SkillInfo`]). One skill's text is
/// `<category>.skill` for all of them. Skills plugins and language plugins list with `describe`;
/// the memory plugin lists its own skills (how to query its store) with `skills`.
pub const CARRIERS: &[(&str, &str)] = &[(super::lang::CONTRACT, "describe"), (CONTRACT, "describe"), (super::memory::CONTRACT, "skills")];

/// The listing method of a skill-carrying contract ([`CARRIERS`]), None for any other contract.
pub fn listing_method(contract: &str) -> Option<&'static str> {
    CARRIERS.iter().find(|(carrier, _)| *carrier == contract).map(|(_, method)| *method)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Description {
    /// `engineering`, ...
    pub id: String,
    pub name: String,
    pub skills: Vec<SkillInfo>,
}
