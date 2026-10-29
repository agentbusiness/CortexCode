//! This node's capability card: a projection of what the frame and the plugins already report,
//! nothing typed by hand. Refreshed on a timer and on change; sent to every connected member.

use cc_protocol::Ctx;
use cc_protocol::contracts::frame::{Machine, PluginInfo, Session, SessionStatus};
use cc_protocol::contracts::lang::Description;
use cc_protocol::contracts::memory::{knowledge, methods as memory_methods};
use cc_protocol::contracts::mesh::{Card, LanguageCard, ModelCard, PluginCard, ProjectCard};
use cc_protocol::contracts::model::{LiveMetrics, ModelInfo, SlotsResult};
use cc_protocol::contracts::now_ms;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// A session that ran within this long counts as a person working here.
const ACTIVE_WINDOW_MS: u64 = 10 * 60 * 1000;

async fn get<T: serde::de::DeserializeOwned>(ctx: &Ctx, method: &str, params: Value) -> Option<T> {
    let value = tokio::time::timeout(Duration::from_secs(5), ctx.call(method, params)).await.ok()?.ok()?;
    serde_json::from_value(value).ok()
}

fn free_disk(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return 0 };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a zeroed statvfs the call fills.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return 0;
    }
    u64::from(stat.f_bavail) * stat.f_frsize
}

fn free_memory() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.available_memory()
}

/// SHA-256 over a plugin's live data folders (relative paths and contents), empty when it has none.
fn data_hash(live_root: Option<&Path>, plugin_id: &str) -> String {
    let Some(live) = live_root else { return String::new() };
    let plugin = live.join("plugins").join(plugin_id);
    let mut files: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    for dir in [plugin.join("data"), plugin.join("backend").join("data")] {
        if !dir.is_dir() {
            continue;
        }
        let mut stack = vec![dir.clone()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(relative) = path.strip_prefix(&plugin) {
                    files.insert(relative.to_string_lossy().into_owned(), path);
                }
            }
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

/// A repository's identity: its root commit (read-only git).
fn git_program() -> Option<std::path::PathBuf> {
    for candidate in ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git"] {
        if Path::new(candidate).is_file() {
            return Some(std::path::PathBuf::from(candidate));
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("git")).find(|git| git.is_file()))
}

fn repository_id(root: &str) -> Option<String> {
    let git = git_program()?;
    let output = std::process::Command::new(git).arg("-C").arg(root).args(["rev-list", "--max-parents=0", "HEAD"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().next().map(|line| line.trim().to_owned()).filter(|id| !id.is_empty())
}

/// The model's load now: busy slots, those a person waits for, the queue.
pub async fn refresh_load(ctx: &Ctx, model: &mut ModelCard) {
    if let Some(slots) = get::<SlotsResult>(ctx, "model.slots", json!({})).await {
        model.slots_busy = slots.busy;
        model.busy_foreground = slots.slots.iter().filter(|slot| slot.busy && slot.priority == Some(cc_protocol::contracts::model::Priority::Foreground)).count() as u32;
        model.queued = slots.queued;
    }
}

pub async fn build(ctx: &Ctx, node: &str, name: &str, home: &Path) -> Card {
    let machine: Machine = get(ctx, "frame.machine", json!({})).await.unwrap_or_default();
    let plugins: Vec<PluginInfo> = get::<Value>(ctx, "frame.plugins.list", json!({})).await.and_then(|v| serde_json::from_value(v.get("plugins").cloned().unwrap_or(Value::Null)).ok()).unwrap_or_default();
    let live_root = ctx.env.live_root.clone();
    let plugin_cards: Vec<PluginCard> = plugins
        .iter()
        .filter(|plugin| !plugin.instance.starts_with("ws-"))
        .map(|plugin| PluginCard {
            id: if plugin.plugin.is_empty() { plugin.plugin_id.clone() } else { plugin.plugin.clone() },
            category: plugin.category.clone(),
            // The installed package's version (with its build), not the plugin's bare semver: two
            // nodes tell exactly which build each one runs.
            version: if plugin.build.is_empty() { plugin.version.clone() } else { plugin.build.clone() },
            data_hash: data_hash(live_root.as_deref(), if plugin.plugin.is_empty() { &plugin.plugin_id } else { &plugin.plugin }),
            active: plugin.state == "active",
        })
        .collect();
    let mut languages = Vec::new();
    for plugin in plugins.iter().filter(|plugin| plugin.contract == cc_protocol::contracts::lang::CONTRACT && plugin.state == "active") {
        if let Some(description) = get::<Description>(ctx, &format!("{}.describe", plugin.category), json!({})).await {
            let version = description.server.as_ref().and_then(|server| server.version.clone()).unwrap_or_default();
            languages.push(LanguageCard { id: description.id, version });
        }
    }
    let model = match get::<ModelInfo>(ctx, "model.info", json!({})).await {
        Some(info) => {
            let slots: Option<SlotsResult> = get(ctx, "model.slots", json!({})).await;
            let metrics: Option<LiveMetrics> = get(ctx, "model.metrics", json!({})).await;
            let plugin = plugins.iter().find(|plugin| plugin.category == "model" && plugin.state == "active").map(|plugin| plugin.plugin_id.clone()).unwrap_or_default();
            Some(ModelCard {
                plugin,
                model_id: info.model_id,
                variant: info.variant,
                family: info.family,
                tags: info.tags,
                ctx_total: info.ctx_total,
                slots: info.slots,
                slots_busy: slots.as_ref().map(|slots| slots.busy).unwrap_or(0),
                busy_foreground: slots.as_ref().map(|slots| slots.slots.iter().filter(|slot| slot.busy && slot.priority == Some(cc_protocol::contracts::model::Priority::Foreground)).count() as u32).unwrap_or(0),
                queued: slots.as_ref().map(|slots| slots.queued).unwrap_or(0),
                tok_s: metrics.map(|metrics| metrics.decode_tps_per_stream).unwrap_or(0.0),
            })
        }
        None => None,
    };
    let sessions: Vec<Session> = get::<Value>(ctx, "frame.session.list", json!({})).await.and_then(|v| serde_json::from_value(v.get("sessions").cloned().unwrap_or(Value::Null)).ok()).unwrap_or_default();
    let now = now_ms();
    let running = sessions.iter().filter(|session| session.status == SessionStatus::Running).count() as u32;
    let agents_running = sessions.iter().filter(|session| session.status == SessionStatus::Running && session.agent.is_some()).count() as u32;
    let user_active = sessions.iter().any(|session| session.agent.is_none() && now.saturating_sub(session.updated_ms) < ACTIVE_WINDOW_MS);
    let agent_limit = get::<Value>(ctx, "agents.scheduler", json!({})).await.and_then(|v| v.get("max_parallel").and_then(Value::as_u64)).unwrap_or(0) as u32;
    let docker = get::<Value>(ctx, "container.list", json!({})).await.is_some();
    let projects: Vec<Value> = get::<Value>(ctx, "frame.project.list", json!({})).await.and_then(|v| v.get("projects").and_then(Value::as_array).cloned()).unwrap_or_default();
    let repositories: Vec<String> = projects.iter().filter_map(|project| project.get("root").and_then(Value::as_str)).filter_map(repository_id).collect();
    let mut project_cards = Vec::new();
    for project in &projects {
        let Some(id) = project.get("id").and_then(Value::as_str) else { continue };
        let name = project.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let created_ms = project.get("created_ms").and_then(Value::as_u64).unwrap_or(0);
        let flows = project.get("flows").and_then(|flows| serde_json::from_value(flows.clone()).ok()).unwrap_or_else(cc_protocol::contracts::frame::flows::default_flows);
        project_cards.push(ProjectCard { id: id.to_owned(), name, description: description(ctx, id).await, created_ms, flows });
    }
    // The flows running here: the active plugins of the chat flow family.
    let mut flows: Vec<String> = plugins.iter().filter(|plugin| plugin.state == "active").filter_map(|plugin| cc_protocol::contracts::frame::flows::flow_of(&plugin.category).map(str::to_owned)).collect();
    flows.sort();
    flows.dedup();
    let on_mains = on_mains();
    Card {
        node: node.to_owned(),
        name: name.to_owned(),
        software: "cortexcode".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        flows,
        chip: machine.chip,
        memory_bytes: machine.memory_bytes,
        cpu_cores: machine.cpu_cores,
        gpu_cores: machine.gpu_cores,
        os_version: machine.os_version,
        free_disk_bytes: free_disk(home),
        free_memory_bytes: free_memory(),
        on_mains,
        user_active,
        model,
        plugins: plugin_cards,
        languages,
        docker,
        sessions_running: running,
        agents_running,
        agent_limit,
        repositories,
        projects: project_cards,
        updated_ms: now,
    }
}

/// The project's description from its memory: its opening paragraph (the sidebar shows a line).
async fn description(ctx: &Ctx, project_id: &str) -> String {
    let listed = get::<Value>(ctx, memory_methods::KNOWLEDGE_LIST, json!({"project_id": project_id, "kind": knowledge::DESCRIPTION})).await;
    let text = listed.as_ref().and_then(|value| value.get("items")).and_then(Value::as_array).and_then(|items| items.first()).and_then(|item| item.get("text")).and_then(Value::as_str).unwrap_or_default();
    let paragraph = text.trim().split("\n\n").next().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
    match paragraph.char_indices().nth(400) {
        Some((cut, _)) => format!("{}…", paragraph[..cut].trim_end()),
        None => paragraph,
    }
}

/// Whether the machine runs on mains power (macOS `pmset -g batt`: "AC Power"; Linux: a mains
/// supply in /sys/class/power_supply that is online); true when it cannot be told (a desktop has
/// no battery).
#[cfg(not(target_os = "macos"))]
fn on_mains() -> bool {
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else { return true };
    let read = |dir: &std::path::Path, file: &str| std::fs::read_to_string(dir.join(file)).unwrap_or_default().trim().to_owned();
    let mains: Vec<_> = supplies.flatten().map(|entry| entry.path()).filter(|dir| read(dir, "type") == "Mains").collect();
    mains.is_empty() || mains.iter().any(|dir| read(dir, "online") == "1")
}

#[cfg(target_os = "macos")]
fn on_mains() -> bool {
    match std::process::Command::new("/usr/bin/pmset").args(["-g", "batt"]).output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            !text.contains("Battery Power")
        }
        Err(_) => true,
    }
}
