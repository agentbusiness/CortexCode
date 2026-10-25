//! The session's cached prefix: system prompt plus tool specs, built once and frozen in the journal
//! (`context` entry) so every call of the session, and any successor after a hot-swap, sends
//! byte-identical bytes ahead of the conversation.
//!
//! Every session of a project and role builds the same prefix from the same sources (policy, the
//! role's stage protocol, tools guide, project rules, style, an unfocused repository map): chat
//! sessions share one, background agents another. The skills catalog is not in the frozen text: a
//! chat session's system message appends it ([`Prefix::system_message`], `limits.skills_in_prefix`)
//! so the turn's first call does not re-read it, else the stages that choose skills show it in their
//! stage message ([`crate::prompts::skills_list`]).

use crate::data::Dataset;
use crate::host::Host;
use crate::journal::{Journal, extra_kinds};
use crate::prompts::{PrefixParts, system_prompt};
use cc_protocol::RpcError;
use cc_protocol::contracts::lang::Applies;
use cc_protocol::contracts::memory::{
    FilesResult, KnowledgeBriefResult, ProjectStatus, RuleListResult, StyleListResult, methods as memory_methods,
};
use cc_protocol::contracts::model::{ChatMessage, ModelInfo, ToolSpec, methods as model_methods};
use std::path::Path;
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{SchemasResult, methods as tools_methods};
use std::collections::BTreeMap;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Prefix {
    /// The role the system prompt was built for (`data::ROLES`).
    pub role: String,
    pub system: String,
    pub tools: Vec<ToolSpec>,
    /// Tokens one conversation may use: the model's whole shared pool, at most the model's native
    /// context. The pool is not divided among conversations: each grows into it as it needs, and
    /// the model plugin spills idle ones to disk or queues a request when it is full.
    pub ctx_window: u32,
    /// The session's skill catalog (`skill://<plugin>/<name>`): what a decision's `skills` field
    /// may choose from.
    pub skills: Vec<String>,
    /// The catalog's lines (`skill://<plugin>/<name>  <summary>`), as the choosing stages show it.
    pub skill_lines: Vec<String>,
    /// `skill://<plugin>/<name>` -> the command that says whether that skill's work is finished
    /// (`SkillInfo::done`): a turn whose work it says is open goes on instead of ending on a reply.
    pub skill_done: BTreeMap<String, String>,
}

impl Prefix {
    /// A skill's catalog line (`<uri>  <summary>`), or its uri when the catalog has no line for it.
    pub fn skill_line(&self, uri: &str) -> String {
        self.skill_lines.iter().find(|line| line.split_whitespace().next() == Some(uri)).cloned().unwrap_or_else(|| uri.to_owned())
    }

    /// Whether this session's skills catalog is part of its system message
    /// (`limits.skills_in_prefix`, chat sessions that offer a choice).
    pub fn catalog_in_prefix(&self, data: &Dataset) -> bool {
        data.limits.skills_in_prefix && data.limits.skills_per_turn > 0 && self.role == crate::data::ROLE_CHAT && !self.skills.is_empty()
    }

    /// The message every call of the session starts with: the system prompt and, when the catalog
    /// is kept in the prefix, the skills catalog in the session's order. Byte-stable per session
    /// (both are frozen in the `context` entry), so it stays in the model's cache across turns.
    pub fn system_message(&self, data: &Dataset) -> ChatMessage {
        if !self.catalog_in_prefix(data) {
            return ChatMessage::new("system", self.system.clone());
        }
        let lines: Vec<String> = self.skills.iter().map(|uri| self.skill_line(uri)).collect();
        ChatMessage::new("system", format!("{}\n\n{}", self.system, crate::prompts::skills_list(data, &lines)))
    }

    fn from_entry(data: &Value) -> Option<Self> {
        let strings = |field: &str| data.get(field).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        Some(Self {
            role: data.get("role")?.as_str()?.to_owned(),
            system: data.get("system")?.as_str()?.to_owned(),
            tools: serde_json::from_value(data.get("tools")?.clone()).ok()?,
            // `ctx_slot`: journals written before the window was the whole pool.
            ctx_window: data.get("ctx_window").or_else(|| data.get("ctx_slot")).and_then(Value::as_u64).unwrap_or(0) as u32,
            skills: strings("skills"),
            skill_done: data.get("skill_done").and_then(Value::as_object).map(|map| map.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()).unwrap_or_default(),
            skill_lines: strings("skill_lines"),
        })
    }
}

/// Facts about the model, read once per process.
pub async fn model_info(host: &Host) -> Option<ModelInfo> {
    host.get::<ModelInfo>(model_methods::INFO, json!({})).await.ok()
}

/// Tokens one conversation may use: the whole pool, at most the native context, at most `cap`
/// (`limits.context_tokens`; 0 = no cap). 0: unknown.
pub fn conversation_window(ctx_total: u32, ctx_native: u32, cap: u32) -> u32 {
    let window = if ctx_native == 0 { ctx_total } else { ctx_total.min(ctx_native) };
    if cap == 0 { window } else { window.min(cap) }
}

/// Retained for callers compiled against older chatflow versions; prefixes contain no map.
#[deprecated(note = "repository maps are excluded from prefixes; the budget is always zero")]
pub fn repo_map_budget(_data: &Dataset, _ctx_window: u32) -> u32 {
    0
}

/// Make sure the memory plugin has the project open (idempotent on its side).
pub async fn open_project(host: &Host, project_id: &str, root: &str) {
    // Indexing a large repository on first open takes a while; the reads that follow need it.
    let opened = tokio::time::timeout(
        Duration::from_secs(600),
        host.call(memory_methods::PROJECT_OPEN, json!({"root": root, "project_id": project_id})),
    )
    .await;
    match opened {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(%error, "memory.project.open failed; reads will report it"),
        Err(_) => tracing::warn!("memory.project.open did not finish in 10 minutes; continuing"),
    }
}

/// What the code graph knows of the project that skills' conditions ([`Applies`]) are matched
/// against: its languages (empty: not indexed yet, or no code), the packages its code imports or
/// its manifests declare, and its files (read only when a condition names files).
#[derive(Debug, Default)]
struct ProjectFacts {
    languages: Vec<String>,
    packages: Vec<String>,
    files: Option<Vec<String>>,
}

impl ProjectFacts {
    async fn read(host: &Host, project_id: &str) -> Self {
        let (mut languages, packages) = match host.get::<ProjectStatus>(memory_methods::PROJECT_STATUS, json!({"project_id": project_id})).await {
            Ok(status) => (status.languages, status.packages),
            Err(_) => (Vec::new(), Vec::new()),
        };
        languages.sort();
        Self { languages, packages, files: None }
    }

    /// Read the project's file paths from the code graph, once.
    async fn load_files(&mut self, host: &Host, project_id: &str) {
        if self.files.is_some() {
            return;
        }
        let files = host.get::<FilesResult>(memory_methods::FILES, json!({"project_id": project_id})).await;
        self.files = Some(files.map(|result| result.paths).unwrap_or_else(|error| {
            tracing::warn!(%error, "project files unavailable; skills that name files are offered by their other conditions");
            Vec::new()
        }));
    }

    /// Whether a skill applies to the project: a project the graph knows no language of yet (new,
    /// empty, still indexing) is offered every skill; otherwise one of the skill's conditions must
    /// hold (a skill without conditions applies everywhere).
    async fn offers(&mut self, host: &Host, project_id: &str, applies: &Applies) -> bool {
        if self.languages.is_empty() || applies.is_empty() {
            return true;
        }
        if !applies.files.is_empty() {
            self.load_files(host, project_id).await;
        }
        applies.matches(&self.languages, self.files.as_deref().unwrap_or_default(), &self.packages)
    }
}

/// The skills of the skills plugins and of the language plugins whose languages the project has (the
/// code graph says which), as `skill://<plugin>/<name>  <summary>` lines, each only when the
/// project shows one of the conditions its plugin declares for it (`data/skills/applies.toml`). A
/// project the graph knows no language of yet is offered every skill; the prefix is rebuilt once the
/// graph names its languages.
/// Skills the project gets: the choosable ones as catalog lines, and the always-loaded ones
/// (`always` in their plugin's applies.toml) as (category, name), which go into the system message.
#[derive(Debug, Default)]
struct ProjectSkills {
    lines: Vec<String>,
    always: Vec<(String, String)>,
    /// `skill://<plugin>/<name>` -> the command that says whether that skill's work is finished.
    done: BTreeMap<String, String>,
}

async fn project_skills(host: &Host, project_id: &str, project: &mut ProjectFacts) -> ProjectSkills {
    let Ok(listed) = host.get::<Value>("frame.plugins.list", json!({})).await else { return ProjectSkills::default() };
    let mut lines = Vec::new();
    let mut always = Vec::new();
    let mut done = BTreeMap::new();
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        let contract = field("contract");
        let Some(listing) = cc_protocol::contracts::skills::listing_method(&contract) else { continue };
        if field("state") != "active" {
            continue;
        }
        let category = field("category");
        let skills = if contract != cc_protocol::contracts::lang::CONTRACT {
            // General know-how (skills plugins, the memory plugin's own): each skill's own
            // conditions decide.
            match host.get::<cc_protocol::contracts::skills::Description>(&format!("{category}.{listing}"), json!({})).await {
                Ok(description) => description.skills,
                Err(_) => continue,
            }
        } else {
            let Ok(description) = host.get::<cc_protocol::contracts::lang::Description>(&format!("{category}.{listing}"), json!({})).await else { continue };
            if !project.languages.is_empty() && !description.languages.iter().any(|language| project.languages.contains(&language.name)) {
                continue;
            }
            description.skills
        };
        for skill in skills {
            if !project.offers(host, project_id, &skill.applies).await {
                continue;
            }
            if let Some(command) = &skill.done {
                done.insert(format!("skill://{category}/{}", skill.name), command.clone());
            }
            if skill.applies.always {
                always.push((category.clone(), skill.name));
            } else {
                lines.push(format!("skill://{category}/{}  {}", skill.name, skill.summary));
            }
        }
    }
    lines.sort();
    always.sort();
    ProjectSkills { lines, always, done }
}

/// A skill file's guide: the body after line 1 (the catalog summary) and the blank line.
fn guide_body(text: &str) -> &str {
    let text = text.trim();
    text.split_once("\n\n").map(|(_, body)| body.trim()).unwrap_or(text)
}

/// The always-loaded skills' texts, each under its heading, for the system message (empty when
/// there are none). A skill that cannot be read is left out (logged): the session still works.
async fn always_section(host: &Host, data: &Dataset, always: &[(String, String)]) -> String {
    let mut parts = Vec::new();
    for (category, name) in always {
        match host.get::<Value>(&format!("{category}.skill"), json!({"name": name})).await {
            Ok(skill) => {
                let body = guide_body(skill.get("text").and_then(Value::as_str).unwrap_or(""));
                if !body.is_empty() {
                    parts.push(data.text("section_guide", &[("uri", &format!("skill://{category}/{name}")), ("text", body)]));
                }
            }
            Err(error) => tracing::warn!(%error, skill = %name, "an always-loaded skill could not be read"),
        }
    }
    parts.join("\n\n")
}

/// The session's prefix for `role`: from its `context` entry, or built now and journaled. A prefix
/// built from another data set version, for another role, before the skills last changed
/// (`skills_changed_ms`) or before the project's languages were known is rebuilt once (the new
/// prompts and catalog take effect on the next call); otherwise it stays byte-identical, so the
/// conversation stays in the model's cache across turns.
pub async fn ensure(journal: &mut Journal, project_id: &str, data: &Dataset, role: &str, skills_changed_ms: u64) -> Result<Prefix, RpcError> {
    let project = ProjectFacts::read(journal.host(), project_id).await;
    let languages = project.languages.clone();
    if let Some(entry) = journal.context_entry()
        && entry.data.get("data_version").and_then(Value::as_str) == Some(data.version().as_str())
        && entry.data.get("role").and_then(Value::as_str) == Some(role)
        // The catalog follows the project once: a prefix built before the graph knew the languages
        // is rebuilt when they are known. After that it stays frozen for the session: a language
        // that shows up later (a text file, a build's output) rebuilt it mid-session, the project
        // description came back reworded, and the whole conversation was read again (116k tokens,
        // five minutes) for a catalog that had not changed. A new session takes the new languages.
        && (languages.is_empty() || entry.data.get("languages").and_then(Value::as_array).is_some_and(|known| !known.is_empty()))
        && entry.data.get("created_ms").and_then(Value::as_u64).is_none_or(|created| created > skills_changed_ms)
        && let Some(prefix) = Prefix::from_entry(&entry.data)
    {
        return Ok(prefix);
    }
    // Compaction marks carry over: a rebuilt prefix is read cold anyway, the conversation after it
    // stays as compact as it was.
    let mark = |field: &str| journal.context_entry().and_then(|entry| entry.data.get(field).and_then(Value::as_u64));
    let (folded_through, digested_through) = (mark("folded_through"), mark("digested_through"));
    let (steps_through, settled_through) = (mark("steps_through"), mark(crate::fit::SETTLED));
    let host = journal.host().clone();
    let (prefix, facts) = build_with(&host, project_id, data, role, project).await?;
    journal
        .append(
            extra_kinds::CONTEXT,
            json!({
                "system": prefix.system,
                "tools": prefix.tools,
                "versions": facts.versions,
                "ctx_window": prefix.ctx_window,
                "repo_map_files": facts.repo_map_files,
                "rules": facts.rules,
                "style": facts.style,
                "created_ms": now_ms(),
                "data_version": data.version(),
                "folded_through": folded_through,
                "digested_through": digested_through,
                "steps_through": steps_through,
                "settled_through": settled_through,
                "skills": prefix.skills,
                "skill_lines": prefix.skill_lines,
                "skill_done": prefix.skill_done,
                "role": role,
                "languages": languages,
            }),
        )
        .await?;
    Ok(prefix)
}

/// What a `context` entry records beside the prefix it was built with.
pub struct BuiltFacts {
    pub versions: Value,
    pub repo_map_files: u32,
    pub rules: usize,
    pub style: usize,
    /// Knowledge items the brief holds.
    pub brief_items: usize,
}

/// The prefix for `role` in the project as it is now: what a session without one journals, and what
/// the project's next conversation starts with (prefilled before it exists, `chatflow.prewarm`).
pub async fn build(host: &Host, project_id: &str, data: &Dataset, role: &str) -> Result<Prefix, RpcError> {
    let project = ProjectFacts::read(host, project_id).await;
    Ok(build_with(host, project_id, data, role, project).await?.0)
}

/// The project's own contract as a prompt section: its `AGENTS.md`, else `CLAUDE.md` (a file that
/// is only `@OTHER.md` lines is followed to that file), within `limits.contract_chars`. Empty when
/// the project has none.
fn contract_section(data: &Dataset, root: &Path) -> String {
    if data.limits.contract_chars == 0 {
        return String::new();
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let Ok(mut text) = std::fs::read_to_string(root.join(name)) else { continue };
        let mut file = name.to_owned();
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
        if !lines.is_empty() && lines.iter().all(|line| line.starts_with('@')) {
            let included: Vec<String> = lines.iter().filter_map(|line| std::fs::read_to_string(root.join(line.trim_start_matches('@'))).ok()).collect();
            if included.is_empty() {
                continue;
            }
            file = lines.iter().map(|line| line.trim_start_matches('@')).collect::<Vec<_>>().join(", ");
            text = included.join("\n\n");
        }
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let cut: String = text.chars().take(data.limits.contract_chars).collect();
        return data.text("section_contract", &[("file", &file), ("text", cut.trim())]);
    }
    String::new()
}

/// What the description is written from: the top of the repository (names only), its languages,
/// and its own documents (README, the agent file, build manifests), each cut short. The hash of
/// this text is stored with the description, so it is written again only when they change.
fn describe_inputs(data: &Dataset, root: &Path, languages: &[String]) -> String {
    let mut listing: Vec<String> = std::fs::read_dir(root)
        .map(|read| {
            read.flatten()
                .filter_map(|item| {
                    let name = item.file_name().to_string_lossy().into_owned();
                    (!name.starts_with('.') && name != "node_modules" && name != "target").then(|| if item.path().is_dir() { format!("{name}/") } else { name })
                })
                .collect()
        })
        .unwrap_or_default();
    listing.sort();
    listing.truncate(80);
    let mut documents = String::new();
    let candidates = ["README.md", "README", "README.rst", "AGENTS.md", "CLAUDE.md", "package.json", "Cargo.toml", "pyproject.toml", "go.mod", "pom.xml", "build.gradle", "CMakeLists.txt", "Makefile"];
    for name in candidates {
        if let Ok(text) = std::fs::read_to_string(root.join(name)) {
            let cut: String = text.chars().take(if name.starts_with("README") { 4000 } else { 1500 }).collect();
            documents.push_str(&data.text("describe_document", &[("name", name), ("text", cut.trim())]));
        }
    }
    data.text("describe_inputs", &[("listing", &listing.join("\n")), ("languages", &languages.join(", ")), ("documents", documents.trim())])
}

/// The project's description (a `description` knowledge item): written by the model when the
/// project has none, or when the documents it was written from changed. A failure leaves the
/// project without one (logged): sessions still work.
async fn ensure_description(host: &Host, data: &Dataset, project_id: &str, root: &Path, languages: &[String]) {
    let inputs = describe_inputs(data, root, languages);
    let hash = format!("inputs:{:016x}", crate::repetition::fingerprint(&inputs));
    let existing = host
        .get::<Value>(memory_methods::KNOWLEDGE_LIST, json!({"project_id": project_id, "kind": cc_protocol::contracts::memory::knowledge::DESCRIPTION}))
        .await
        .ok()
        .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    if existing.iter().any(|item| item.get("evidence").and_then(Value::as_array).is_some_and(|evidence| evidence.iter().any(|e| e.as_str() == Some(hash.as_str())))) {
        return;
    }
    let messages = vec![
        ChatMessage::new("system", data.prompt("describe").to_owned()),
        ChatMessage::new("user", inputs.clone()),
    ];
    let params = json!({
        "messages": messages,
        "tools": [],
        "sampling": data.sampling("DESCRIBE", false),
        "thinking": crate::llm::thinking("none"),
        "priority": "foreground",
        "max_tokens": 700,
    });
    match host.get::<cc_protocol::contracts::model::ChatResult>(model_methods::CHAT, params).await {
        Ok(result) if !result.content.trim().is_empty() => {
            let upsert = json!({
                "project_id": project_id, "kind": cc_protocol::contracts::memory::knowledge::DESCRIPTION, "name": "project",
                "text": result.content.trim(), "source": "inferred", "by": "chatflow", "evidence": [hash],
            });
            if let Err(error) = host.effect(memory_methods::KNOWLEDGE_UPSERT, upsert).await {
                tracing::warn!(%error, "the project description could not be stored");
            }
        }
        Ok(_) => tracing::warn!("the project description came back empty"),
        Err(error) => tracing::warn!(%error, "the project description could not be written"),
    }
}

async fn build_with(host: &Host, project_id: &str, data: &Dataset, role: &str, mut project: ProjectFacts) -> Result<(Prefix, BuiltFacts), RpcError> {
    let root = host.project_get(project_id).await.map(|project| project.root).unwrap_or_default();
    // The project's folder: an offline one gets a tools guide that says so before any call.
    let schemas: SchemasResult = host.get(tools_methods::SCHEMAS, json!({"workspace": root})).await?;
    let info = model_info(host).await;
    let ctx_window = info.as_ref().map(|info| conversation_window(info.ctx_total, info.ctx_native, data.limits.context_tokens)).unwrap_or(0);

    let mut rules = Vec::new();
    for scope in ["global", project_id] {
        if let Ok(list) = host.get::<RuleListResult>(memory_methods::RULE_LIST, json!({"scope": scope})).await {
            rules.extend(list.rules.into_iter().map(|rule| rule.text));
        }
    }
    let mut style = Vec::new();
    for scope in ["global", project_id] {
        if let Ok(list) = host.get::<StyleListResult>(memory_methods::STYLE_LIST, json!({"scope": scope})).await {
            style.extend(list.entries.into_iter().map(|entry| format!("{}: {}", entry.area, entry.text)));
        }
    }
    // No repository map in the prompt: on a large codebase a map of signatures is noise and a
    // misleading cue. The prompt carries what the software is (the description, in the brief) and
    // the project's own contract; code is found per task by searching for its topic.
    let (map_text, map_files) = (String::new(), 0);
    let contract = if root.is_empty() { String::new() } else { contract_section(data, Path::new(&root)) };
    if !root.is_empty() && data.limits.brief_tokens > 0 {
        ensure_description(host, data, project_id, Path::new(&root), &project.languages).await;
    }
    // What the project knows (docs/PROJECT-KNOWLEDGE-PLAN.md P4): accepted decisions, the user's
    // facts, the concepts that bind most code, open questions, within `limits.brief_tokens`, as the
    // memory plugin composes and counts it. Computed once per session with the rest of the prefix,
    // so it stays byte-stable and cached; the next session sees what was learned since.
    let brief = if data.limits.brief_tokens == 0 {
        KnowledgeBriefResult::default()
    } else {
        host.get::<KnowledgeBriefResult>(memory_methods::KNOWLEDGE_BRIEF, json!({"project_id": project_id, "budget_tokens": data.limits.brief_tokens})).await.unwrap_or_default()
    };
    let ProjectSkills { lines: skills, always, done: skill_done } = project_skills(host, project_id, &mut project).await;
    let mut system = system_prompt(data, role, &PrefixParts { project_root: &root, tools_guide: &schemas.guide, rules: &rules, style: &style, brief: &brief.text, contract: &contract, repo_map: &map_text });
    // The project's language guides: every session in the project (chat and agents) works with
    // them, so they sit in the byte-stable, cached system message, not in a choice.
    let guides = always_section(host, data, &always).await;
    if !guides.is_empty() {
        system = format!("{system}\n\n{guides}");
    }
    let catalog: Vec<String> = skills.iter().filter_map(|line| line.split_whitespace().next()).map(str::to_owned).collect();
    let prefix = Prefix { role: role.to_owned(), system, tools: schemas.tools, ctx_window, skills: catalog, skill_lines: skills, skill_done };
    let facts = BuiltFacts { versions: json!(schemas.versions), repo_map_files: map_files, rules: rules.len(), style: style.len(), brief_items: brief.ids.len() };
    Ok((prefix, facts))
}
