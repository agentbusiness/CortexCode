//! The plugin's own skills: know-how for the model about this store (how to query IronGraph),
//! shipped in the folder's `data/skills` in the skills format of the language and skills plugins
//! (`cc_lang_kit::skills`). `memory.skills` lists them for the skill catalog and `memory.skill`
//! serves one (`skill://memory/<name>`), so they leave with the plugin when another store replaces it.
//! Both read the folder in use at the time: the live one in the plugin's folder of the code folder
//! when it exists, else the packaged one; [`watch`] tells the other plugins when it changes.

use cc_lang_kit::skills;
use cc_protocol::contracts::skills::{Description, SkillInfo, SkillParams, SkillResult};
use cc_protocol::{Ctx, PluginEnv, RpcError, codes};
use std::path::PathBuf;
use std::sync::OnceLock;

/// `data/skills` beside the executable (the frame's store copies the asset there) or in the
/// plugin folder above the build directory.
fn packaged() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(skills::locate_dir).clone()
}

/// The skills folder to read now ([`skills::current_dir`]).
fn dir(env: &PluginEnv) -> Result<PathBuf, RpcError> {
    skills::current_dir(env, packaged().as_deref()).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "memory: the data/skills directory is missing"))
}

pub fn describe(env: &PluginEnv) -> Result<Description, RpcError> {
    let listed = skills::load(&dir(env)?).map_err(|error| RpcError::new(codes::UNAVAILABLE, error))?;
    Ok(Description {
        id: "memory".into(),
        name: "Memory (IronGraph)".into(),
        skills: listed.into_iter().map(|skill| SkillInfo { name: skill.name, summary: skill.summary, codes: skill.codes, applies: skill.applies, done: skill.done, units: skill.units, unit_done: skill.unit_done }).collect(),
    })
}

pub fn skill(env: &PluginEnv, params: SkillParams) -> Result<SkillResult, RpcError> {
    let text = skills::read(&dir(env)?, &params.name)?;
    Ok(SkillResult { name: params.name, text })
}

/// Publish `skills.changed` for `memory` when the skills in use change ([`skills::watch`]).
pub fn watch(ctx: Ctx) {
    skills::watch(ctx, "memory".into(), packaged());
}
