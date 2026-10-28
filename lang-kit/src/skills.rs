//! Skills: Markdown files a language plugin ships in `data/skills/<name>.md` (the file stem is the
//! skill's name). Line 1 is a one-sentence summary of when the skill helps (plain text; a leading
//! `#` heading marker is tolerated and not part of it), then a blank line, then the Markdown body.
//! `skill` returns the whole file.
//!
//! Beside the skills, `data/skills/codes.toml` may say which diagnostic codes each skill covers, as
//! the tools emit them (a language server's diagnostic `code`, a compiler's or linter's code in its
//! output): one key per skill, its value the list of codes. The model never reads this file; the
//! tools plugin attaches a skill to a result that shows one of its codes. [`load`] validates it:
//! every key is a skill of the directory, every code is a non-empty word declared by one skill only.
//!
//! ```toml
//! rust-borrow-checker = ["E0382", "E0499", "E0502"]
//! ```
//!
//! `data/skills/applies.toml` may say which projects a skill can help in: one table per skill with
//! any of `languages` (as the code graph names them), `files` (repository path globs, one of which
//! must exist) and `imports` (packages the code imports or the manifests declare). A project that
//! shows none of a skill's conditions is not offered the skill; a skill without a table applies to
//! every project. `always = true` makes a skill load into every session of a project it applies to
//! (a language plugin's guide to its language), never offered as a choice. [`load`] validates it:
//! every table is a skill of the directory, every key one of the five, every list non-empty and
//! every item a non-empty word. `done = "<command>"` names the command whose exit status says
//! whether the skill's work is finished (a skill that drives a procedure through a tool); the
//! chat flow keeps a turn going while it says the work is open. `units = "<command>"` lists the
//! independent units the work falls into (`<id>\t<title>` per line) and `unit_done = "<command
//! with {unit}>"` says when one unit is finished: the chat flow may then run every unit as an
//! agent of its own at once, and one last task to finish the whole.
//!
//! ```toml
//! [sql]
//! files = ["*.sql"]
//! imports = ["sqlite3", "psycopg2", "database/sql"]
//! ```
//!
//! The plugin folder declares `data/` as its data (`[data] dirs = ["data"]`): a packed `.ccplugin`
//! holds it at `data/`, one level above the packed executable (`backend/`). Run from the build
//! directory (`backend/target/release/`), the folder's own `data/` is three levels up. That
//! packaged copy serves until the plugin's folder in the live folder has its own `data/skills` (the
//! live skills, see [`current_dir`]); [`watch`] tells the other plugins when the skills in use
//! change.

use cc_protocol::contracts::lang::Applies;
use cc_protocol::{Ctx, PluginEnv, RpcError};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The file beside the skills that maps skill names to the diagnostic codes they cover.
pub const CODES_FILE: &str = "codes.toml";

/// The file beside the skills that says which projects each skill applies to.
pub const APPLIES_FILE: &str = "applies.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub summary: String,
    pub path: PathBuf,
    /// The diagnostic codes the skill covers ([`CODES_FILE`]); empty from [`list`].
    pub codes: Vec<String>,
    /// The projects the skill applies to ([`APPLIES_FILE`]); empty (everywhere) from [`list`].
    pub applies: Applies,
    /// The command that says whether the skill's work is finished ([`APPLIES_FILE`] `done`).
    pub done: Option<String>,
    /// The command that lists the skill's independent units ([`APPLIES_FILE`] `units`).
    pub units: Option<String>,
    /// The `done` of one unit, `{unit}` standing for its id ([`APPLIES_FILE`] `unit_done`).
    pub unit_done: Option<String>,
}

/// The live skills, relative to the plugin's folder in the live folder.
pub const LIVE_DIR: &str = "data/skills";

/// Published when the skills in use changed (`{"category"}`).
pub const CHANGED_TOPIC: &str = cc_protocol::contracts::skills::topics::CHANGED;

/// How often [`watch`] looks at the skills folder (file metadata only).
pub const WATCH_EVERY: Duration = Duration::from_secs(1);

/// `data/skills` beside the running executable or in one of its ancestors (at most four levels up).
pub fn locate_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    locate_from(exe.parent()?)
}

/// `data/skills` in `start` or one of its ancestors (at most four levels up).
pub fn locate_from(start: &Path) -> Option<PathBuf> {
    start.ancestors().take(5).map(|dir| dir.join("data").join("skills")).find(|candidate| candidate.is_dir())
}

/// The skills folder to read now: the live one ([`LIVE_DIR`] in the plugin's folder of the code
/// folder) when it exists, else `packaged`; None when the plugin ships no skills. Resolved on every
/// read, since the live folder may appear after the plugin started.
pub fn current_dir(env: &PluginEnv, packaged: Option<&Path>) -> Option<PathBuf> {
    packaged.map(|packaged| env.live_or(LIVE_DIR, packaged.to_path_buf()))
}

/// What [`watch`] compares: the folder, and the name, size and modification time of every skill
/// and declaration file in it.
type Signature = Option<(PathBuf, Vec<(String, u64, SystemTime)>)>;

fn signature(dir: Option<PathBuf>) -> Signature {
    let dir = dir?;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.ends_with(".md") || name.ends_with(".toml")) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            files.push((name, meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        }
    }
    files.sort();
    Some((dir, files))
}

/// Publish [`CHANGED_TOPIC`] for `category` whenever the skills folder in use ([`current_dir`] of
/// `packaged`) changes: a skill or declaration file edited, added or removed, or the live folder
/// appearing. Runs until the plugin exits.
pub fn watch(ctx: Ctx, category: String, packaged: Option<PathBuf>) {
    if packaged.is_none() {
        return;
    }
    tokio::spawn(async move {
        let now = || signature(current_dir(&ctx.env, packaged.as_deref()));
        let mut known = now();
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            let seen = now();
            if seen != known {
                known = seen;
                ctx.publish(CHANGED_TOPIC, json!({"category": category}));
            }
        }
    });
}

/// The summary line of a skill's text.
pub fn summary_of(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    first.trim_start_matches('#').trim().to_owned()
}

/// A skill name is the file stem: lowercase letters, digits, `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Every `*.md` in `dir` with a valid name, by name.
pub fn list(dir: &Path) -> Result<Vec<Skill>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".md")) else { continue };
        if !valid_name(name) || !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        skills.push(Skill { name: name.to_owned(), summary: summary_of(&text), path, codes: Vec::new(), applies: Applies::default(), done: None, units: None, unit_done: None });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// Every skill of `dir` ([`list`]) with the codes [`CODES_FILE`] declares for it and the
/// conditions [`APPLIES_FILE`] declares for it (none when a file is absent). An invalid file is an
/// error naming every problem.
pub fn load(dir: &Path) -> Result<Vec<Skill>, String> {
    let mut skills = list(dir)?;
    let names: Vec<String> = skills.iter().map(|skill| skill.name.clone()).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    if let Some((path, text)) = read_beside(dir, CODES_FILE)? {
        let mut codes = parse_codes(&text, &names).map_err(|problem| format!("{}: {problem}", path.display()))?;
        for skill in &mut skills {
            skill.codes = codes.remove(&skill.name).unwrap_or_default();
        }
    }
    if let Some((path, text)) = read_beside(dir, APPLIES_FILE)? {
        let mut applies = parse_applies(&text, &names).map_err(|problem| format!("{}: {problem}", path.display()))?;
        for skill in &mut skills {
            if let Some((conditions, commands)) = applies.remove(&skill.name) {
                skill.applies = conditions;
                skill.done = commands.done;
                skill.units = commands.units;
                skill.unit_done = commands.unit_done;
            }
        }
    }
    Ok(skills)
}

/// The text of the file `name` in `dir`, None when it does not exist.
fn read_beside(dir: &Path, name: &str) -> Result<Option<(PathBuf, String)>, String> {
    let path = dir.join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// The text of an [`APPLIES_FILE`], checked against the skills that exist (`skills`): skill name
/// -> its conditions (lists in the file's order) and its `done` command.
/// The commands a skill's table may name: `done`, `units`, `unit_done`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SkillCommands {
    pub done: Option<String>,
    pub units: Option<String>,
    pub unit_done: Option<String>,
}

pub fn parse_applies(text: &str, skills: &[&str]) -> Result<BTreeMap<String, (Applies, SkillCommands)>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|error| format!("not valid TOML: {}", error.message()))?;
    let mut applies = BTreeMap::new();
    let mut problems = Vec::new();
    for (name, value) in table {
        if !skills.contains(&name.as_str()) {
            problems.push(format!("`{name}` is not a skill here (no {name}.md)"));
            continue;
        }
        let Some(fields) = value.as_table() else {
            problems.push(format!("`{name}` must be a table ([{name}] with languages, files or imports)"));
            continue;
        };
        if fields.is_empty() {
            problems.push(format!("`{name}` declares no condition"));
        }
        let mut conditions = Applies::default();
        let mut commands = SkillCommands::default();
        for (key, value) in fields {
            if let Some(slot) = match key.as_str() {
                "done" => Some(&mut commands.done),
                "units" => Some(&mut commands.units),
                "unit_done" => Some(&mut commands.unit_done),
                _ => None,
            } {
                match value.as_str() {
                    Some(command) if !command.trim().is_empty() => *slot = Some(command.trim().to_owned()),
                    _ => problems.push(format!("`{name}.{key}` must be a non-empty command")),
                }
                continue;
            }
            if key == "always" {
                match value.as_bool() {
                    Some(flag) => conditions.always = flag,
                    None => problems.push(format!("`{name}.always` must be true or false")),
                }
                continue;
            }
            let list = match key.as_str() {
                "languages" => &mut conditions.languages,
                "files" => &mut conditions.files,
                "imports" => &mut conditions.imports,
                _ => {
                    problems.push(format!("`{name}.{key}` is not a condition (languages, files, imports, always, done, units or unit_done)"));
                    continue;
                }
            };
            let Some(items) = value.as_array() else {
                problems.push(format!("`{name}.{key}` must be a list"));
                continue;
            };
            if items.is_empty() {
                problems.push(format!("`{name}.{key}` is empty"));
            }
            for item in items {
                match item.as_str() {
                    Some(word) if !word.is_empty() && !word.chars().any(char::is_whitespace) => {
                        if list.iter().any(|seen| seen == word) {
                            problems.push(format!("`{name}.{key}` lists `{word}` twice"));
                        } else {
                            list.push(word.to_owned());
                        }
                    }
                    _ => problems.push(format!("`{name}.{key}`: {item} is not a non-empty string without spaces")),
                }
            }
        }
        if commands.unit_done.as_ref().is_some_and(|command| !command.contains("{unit}")) {
            problems.push(format!("`{name}.unit_done` must contain {{unit}}"));
        }
        applies.insert(name, (conditions, commands));
    }
    if problems.is_empty() { Ok(applies) } else { Err(problems.join("; ")) }
}

/// The text of a [`CODES_FILE`], checked against the skills that exist (`skills`): skill name ->
/// codes in the file's order.
pub fn parse_codes(text: &str, skills: &[&str]) -> Result<BTreeMap<String, Vec<String>>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|error| format!("not valid TOML: {}", error.message()))?;
    let mut codes = BTreeMap::new();
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    let mut problems = Vec::new();
    for (name, value) in table {
        if !skills.contains(&name.as_str()) {
            problems.push(format!("`{name}` is not a skill here (no {name}.md)"));
            continue;
        }
        let Some(items) = value.as_array() else {
            problems.push(format!("`{name}` must be a list of codes"));
            continue;
        };
        if items.is_empty() {
            problems.push(format!("`{name}` declares no codes"));
        }
        let mut list = Vec::new();
        for item in items {
            match item.as_str() {
                Some(code) if !code.is_empty() && !code.chars().any(char::is_whitespace) => {
                    if let Some(first) = owner.get(code) {
                        problems.push(format!("code `{code}` is declared by both `{first}` and `{name}`"));
                    } else {
                        owner.insert(code.to_owned(), name.clone());
                        list.push(code.to_owned());
                    }
                }
                _ => problems.push(format!("`{name}`: {item} is not a code (a non-empty string without spaces)")),
            }
        }
        codes.insert(name, list);
    }
    if problems.is_empty() { Ok(codes) } else { Err(problems.join("; ")) }
}

/// One skill's text.
pub fn read(dir: &Path, name: &str) -> Result<String, RpcError> {
    if !valid_name(name) {
        return Err(RpcError::invalid_params(format!("`{name}` is not a skill name (lowercase letters, digits, - and _)")));
    }
    let path = dir.join(format!("{name}.md"));
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(RpcError::not_found(format!("no skill `{name}`"))),
        Err(error) => Err(RpcError::internal(format!("cannot read {}: {error}", path.display()))),
    }
}
