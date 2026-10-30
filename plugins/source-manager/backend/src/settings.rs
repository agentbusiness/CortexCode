//! What the plugin keeps: the backup repository, the shared pull requests and the last update's
//! outcome. It lives in the app's graph (WORKSPACE layer), held in memory while the plugin runs;
//! each change writes only the properties that changed.
//!
//! ```text
//! (:Backup {key: "backup", repo, auto, last_push_ms, last_push_error})-[:ON]->(:Machine)
//! (:PullRequest {key: url, number, branch, commits, state, ms})-[:ON]->(:Machine)
//! (:LiveFolder {key: "live", update, conflicts})-[:ON]->(:Machine)
//! stream `source.pushes`    every backup push {repo, ms, ok, error}
//! ```
//!
//! Earlier versions kept this as one setting (`source.settings`) and before that as a JSON file
//! (`settings.json`): either is moved into the graph once, then deleted. The live folder itself (a
//! git repository of the shipped data) is the app's source, not records: it stays a folder.

use crate::graph::{self, Graph};
use cc_protocol::contracts::memory::methods;
use cc_protocol::contracts::source::Shared;
use cc_protocol::{RpcError, codes};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Settings {
    /// `owner/name` of the backup repository.
    #[serde(default)]
    pub backup_repo: Option<String>,
    /// Push after every change.
    #[serde(default)]
    pub backup_auto: bool,
    #[serde(default)]
    pub last_push_ms: Option<u64>,
    #[serde(default)]
    pub last_push_error: Option<String>,
    /// Pull requests sent to the official repository, oldest first.
    #[serde(default)]
    pub shared: Vec<Shared>,
    /// up_to_date | conflicts | no_source (the last update).
    #[serde(default)]
    pub update: String,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

/// The setting the settings were kept in before the graph.
const OLD_SETTING: &str = "source.settings";
/// The file they were kept in before that.
pub const SETTINGS: &str = "settings.json";
const BACKUP: (&str, &str) = ("Backup", "backup");
const LIVE: (&str, &str) = ("LiveFolder", "live");
const PULL_REQUEST: &str = "PullRequest";
/// The stream of backup pushes, and how long it is kept.
pub const PUSHES: &str = "source.pushes";
pub const PUSHES_DAYS: u32 = 365;

/// Write what differs between `old` and `new`.
pub fn write_changes(graph: &Graph, old: &Settings, new: &Settings) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut backup = Map::new();
    if old.backup_repo != new.backup_repo {
        backup.insert("repo".into(), json!(new.backup_repo));
    }
    if old.backup_auto != new.backup_auto {
        backup.insert("auto".into(), json!(new.backup_auto));
    }
    if old.last_push_ms != new.last_push_ms {
        backup.insert("last_push_ms".into(), json!(new.last_push_ms));
    }
    if old.last_push_error != new.last_push_error {
        backup.insert("last_push_error".into(), json!(new.last_push_error));
    }
    if !backup.is_empty() {
        nodes.push(graph::node(BACKUP.0, BACKUP.1, backup));
        edges.push(graph::edge(BACKUP, "ON", graph::machine()));
    }
    let mut live = Map::new();
    if old.update != new.update {
        live.insert("update".into(), json!(new.update));
    }
    if old.conflicts != new.conflicts {
        live.insert("conflicts".into(), json!(new.conflicts));
    }
    if !live.is_empty() {
        nodes.push(graph::node(LIVE.0, LIVE.1, live));
        edges.push(graph::edge(LIVE, "ON", graph::machine()));
    }
    for pr in &new.shared {
        if old.shared.iter().any(|known| known == pr) {
            continue;
        }
        let props = graph::props([
            ("number", json!(pr.number)),
            ("branch", json!(pr.branch)),
            ("commits", json!(pr.commits)),
            ("state", json!(pr.state)),
            ("ms", json!(pr.ms)),
        ]);
        nodes.push(graph::node(PULL_REQUEST, &pr.url, props));
        edges.push(graph::edge((PULL_REQUEST, &pr.url), "ON", graph::machine()));
    }
    graph.put(nodes, edges);
    let gone: Vec<_> = old.shared.iter().filter(|pr| !new.shared.iter().any(|kept| kept.url == pr.url)).map(|pr| graph::at(PULL_REQUEST, &pr.url)).collect();
    graph.retire(gone, "dropped");
}

/// Move the old setting (or the older file in `data_dir`) into the graph once, then delete it.
pub async fn migrate(graph: &Graph, data_dir: &Path) {
    let file = data_dir.join(SETTINGS);
    let (old, from_setting) = match graph.ask::<Value>(methods::SETTINGS_GET, json!({ "path": OLD_SETTING })).await {
        Ok(setting) => (setting.get("value").cloned().and_then(|value| serde_json::from_value::<Settings>(value).ok()), true),
        Err(error) if error.code == codes::NOT_FOUND => (std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok()), false),
        Err(error) => {
            tracing::warn!(error = %error.message, "the old settings were not read; they are moved at the next start");
            return;
        }
    };
    let Some(old) = old else { return };
    write_changes(graph, &Settings::default(), &old);
    if !graph.flush().await {
        tracing::warn!("the old settings were not moved into the graph; they are kept and tried again at the next start");
        return;
    }
    if from_setting {
        if let Err(error) = graph.ask::<Value>(methods::SETTINGS_DELETE, json!({ "path": OLD_SETTING })).await {
            tracing::warn!(error = %error.message, "the old setting `source.settings` was moved into the graph but not deleted");
        }
    } else {
        let _ = std::fs::remove_file(&file);
    }
    tracing::info!("the source settings moved into the graph");
}

/// Read the settings from the graph.
pub async fn read(graph: &Graph) -> Result<Settings, RpcError> {
    let mut settings = Settings::default();
    let one = |label: &str| format!("MATCH (n:{label}) WHERE n.ended_ms IS NULL RETURN n");
    if let Some(backup) = graph.nodes(&one(BACKUP.0), Map::new(), 1).await?.into_iter().next() {
        settings.backup_repo = Some(graph::text(&backup, "repo")).filter(|repo| !repo.is_empty());
        settings.backup_auto = graph::flag(&backup, "auto");
        settings.last_push_ms = graph::number(&backup, "last_push_ms");
        settings.last_push_error = backup.get("last_push_error").and_then(Value::as_str).map(str::to_owned);
    }
    if let Some(live) = graph.nodes(&one(LIVE.0), Map::new(), 1).await?.into_iter().next() {
        settings.update = graph::text(&live, "update");
        settings.conflicts = graph::texts(&live, "conflicts");
    }
    let mut shared: Vec<Shared> = graph
        .nodes(&one(PULL_REQUEST), Map::new(), 5000)
        .await?
        .into_iter()
        .map(|pr| Shared {
            url: graph::text(&pr, "key"),
            number: graph::number(&pr, "number").unwrap_or_default(),
            branch: graph::text(&pr, "branch"),
            commits: graph::texts(&pr, "commits"),
            state: graph::text(&pr, "state"),
            ms: graph::number(&pr, "ms").unwrap_or_default(),
        })
        .collect();
    shared.sort_by_key(|pr| pr.ms);
    settings.shared = shared;
    Ok(settings)
}
