//! Learned procedure records and owner decisions. All mutable state stays in the app graph.
use crate::graphapi::GraphStore;
use cc_protocol::{
    RpcError,
    contracts::{memory::*, now_ms},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn hash(text: &str) -> String {
    crate::facts::hash_hex(text.as_bytes())
}
pub fn task_key(text: &str) -> String {
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
fn gref(label: &str, key: &str) -> GraphRef {
    GraphRef {
        label: label.into(),
        key: key.into(),
    }
}
fn edge(from: &GraphNode, rel: &str, label: &str, key: &str) -> GraphEdge {
    GraphEdge {
        from: gref(&from.label, &from.key),
        rel: rel.into(),
        to: gref(label, key),
        props: Map::new(),
        temporal: Map::new(),
    }
}
fn node<T: Serialize>(label: &str, key: &str, data: &T) -> Result<GraphNode, RpcError> {
    let mut props =
        object(serde_json::to_value(data).map_err(|e| RpcError::internal(e.to_string()))?);
    // Relationships, not nested copies. API projections may include these collections.
    for field in ["key", "sources", "uses", "history", "episodes"] {
        props.remove(field);
    }
    Ok(GraphNode {
        label: label.into(),
        key: key.into(),
        props,
        temporal: Map::new(),
    })
}
async fn put(
    store: &GraphStore,
    mut nodes: Vec<GraphNode>,
    mut edges: Vec<GraphEdge>,
) -> Result<(), RpcError> {
    let projects: BTreeSet<String> = nodes
        .iter()
        .filter_map(|n| {
            n.props
                .get("project_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let mut known = BTreeSet::new();
    if !projects.is_empty() {
        let rows = store
            .query(GraphQueryParams {
                cypher: "MATCH (p:Project) WHERE p.id IN $ids AND p.ended_ms IS NULL RETURN p.id"
                    .into(),
                params: object(json!({"ids": projects})),
                limit: Some(100),
            })
            .await?;
        known.extend(
            rows.rows
                .iter()
                .filter_map(|r| r.first().and_then(Value::as_str).map(str::to_owned)),
        );
    }
    for node in &mut nodes {
        for prop in ["state", "verdict", "reason"] {
            if let Some(value) = node.props.remove(prop) {
                node.temporal.insert(prop.into(), value);
            }
        }
        if node.label == "ProcedureUse" {
            if let Some(value) = node.props.remove("outcome") {
                node.temporal.insert("outcome".into(), value);
            }
        }
        edges.push(edge(node, "ON", "Machine", &store.machine().key));
        if let Some(project) = node
            .props
            .get("project_id")
            .and_then(Value::as_str)
            .filter(|id| known.contains(*id))
        {
            edges.push(edge(node, "IN", "Project", project));
        }
    }
    store
        .put(GraphPutParams {
            nodes,
            edges,
            layer: GraphLayer::Workspace,
            at_ms: Some(now_ms()),
        })
        .await
        .map(|_| ())
}
async fn read<T: DeserializeOwned>(
    store: &GraphStore,
    query: &str,
    params: Value,
    limit: u32,
) -> Result<Vec<T>, RpcError> {
    let rows = store
        .query(GraphQueryParams {
            cypher: query.into(),
            params: object(params),
            limit: Some(limit),
        })
        .await?;
    rows.rows
        .into_iter()
        .map(|row| {
            let props = row
                .first()
                .and_then(|n| n.get("properties"))
                .ok_or_else(|| RpcError::internal("A procedure record could not be read."))?;
            serde_json::from_value(props.clone()).map_err(|e| {
                RpcError::internal(format!("A procedure record has an unexpected shape: {e}"))
            })
        })
        .collect()
}
async fn revision(
    store: &GraphStore,
    key: &str,
    project: &str,
) -> Result<ProcedureRevision, RpcError> {
    read(store, "MATCH (p:ProcedureRevision {key: $key}) WHERE p.project_id = $project AND p.ended_ms IS NULL RETURN p", json!({"key": key, "project": project}), 1).await?.into_iter().next()
        .ok_or_else(|| RpcError::not_found("This procedure revision is not in this project."))
}
async fn sources(store: &GraphStore, key: &str) -> Result<Vec<ProcedureEpisode>, RpcError> {
    read(store, "MATCH (p:ProcedureRevision {key: $key})-[:DERIVED_FROM]->(e:ProcedureEpisode) WHERE e.ended_ms IS NULL RETURN e ORDER BY e.key", json!({"key": key}), 8).await
}
async fn uses(store: &GraphStore, key: &str) -> Result<Vec<ProcedureUse>, RpcError> {
    read(store, "MATCH (u:ProcedureUse)-[:FOR]->(p:ProcedureRevision {key: $key}) WHERE u.ended_ms IS NULL RETURN u ORDER BY u.created_ms DESC LIMIT 200", json!({"key": key}), 200).await
}

pub async fn record(store: &GraphStore, mut e: ProcedureEpisode) -> Result<(), RpcError> {
    if e.project_id.is_empty()
        || e.session_id.is_empty()
        || e.run_id.is_empty()
        || e.task_key.is_empty()
        || e.text.is_empty()
        || e.text.chars().count() > 1800
        || e.anchor.len() > 100
        || !matches!(
            e.outcome.as_str(),
            "done" | "failed" | "stopped" | "unknown"
        )
    {
        return Err(RpcError::invalid_params(
            "A procedure episode needs a bounded real run, task and lesson.",
        ));
    }
    e.key = hash(&format!(
        "{}\n{}\n{}\n{}\n{}",
        e.session_id, e.run_id, e.lesson_id, e.evidence, e.text
    ));
    let project = store
        .query(GraphQueryParams {
            cypher: "MATCH (p:Project {id: $id}) WHERE p.ended_ms IS NULL RETURN p".into(),
            params: object(json!({"id": e.project_id})),
            limit: Some(1),
        })
        .await?;
    if project.rows.is_empty() {
        return Err(RpcError::not_found(
            "The episode's project is no longer available.",
        ));
    }
    let exists: Vec<ProcedureEpisode> = read(
        store,
        "MATCH (e:ProcedureEpisode {key: $key}) RETURN e",
        json!({"key": e.key}),
        1,
    )
    .await?;
    if let Some(existing) = exists.into_iter().next() {
        e = existing;
    } else {
        e.created_ms = now_ms();
    }
    let n = node("ProcedureEpisode", &e.key, &e)?;
    // Machine is always present; optional records are linked only when they exist.
    let mut links = Vec::new();
    let session = store
        .query(GraphQueryParams {
            cypher: "MATCH (s:Session {id: $id}) RETURN s".into(),
            params: object(json!({"id": e.session_id})),
            limit: Some(1),
        })
        .await?;
    if !session.rows.is_empty() {
        links.push(edge(&n, "FROM", "Session", &e.session_id));
    }
    // Lesson may be in a repository's graph rather than the main graph; its identity remains evidence.
    let lesson = store
        .query(GraphQueryParams {
            cypher: "MATCH (k:Knowledge {id: $id}) RETURN k".into(),
            params: object(json!({"id": e.lesson_id})),
            limit: Some(1),
        })
        .await?;
    if !lesson.rows.is_empty() {
        links.push(edge(&n, "ABOUT", "Knowledge", &e.lesson_id));
    }
    put(store, vec![n], links).await
}

fn material_key(keys: &[String]) -> String {
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    hash(&keys.join("\n"))
}

pub async fn material(store: &GraphStore) -> Result<ProcedureMaterial, RpcError> {
    let episodes: Vec<ProcedureEpisode> = read(store, "MATCH (e:ProcedureEpisode) WHERE e.ended_ms IS NULL RETURN e ORDER BY e.created_ms DESC LIMIT 200", json!({}), 200).await?;
    let mut groups: BTreeMap<(String, String), Vec<ProcedureEpisode>> = BTreeMap::new();
    for e in episodes {
        let group = groups
            .entry((e.project_id.clone(), e.anchor.clone()))
            .or_default();
        if group.len() < 8 {
            group.push(e);
        }
    }
    for ((project, anchor), group) in groups {
        let tasks: BTreeSet<_> = group.iter().map(|e| e.task_key.as_str()).collect();
        let runs: BTreeSet<_> = group.iter().map(|e| (&e.session_id, &e.run_id)).collect();
        if tasks.len() < 2 || runs.len() < 2 {
            continue;
        }
        let key = material_key(&group.iter().map(|e| e.key.clone()).collect::<Vec<_>>());
        let attempted = store
            .query(GraphQueryParams {
                cypher: "MATCH (a:ProcedureAttempt {key: $key}) WHERE a.ready = true RETURN a"
                    .into(),
                params: object(json!({"key": key})),
                limit: Some(1),
            })
            .await?;
        if !attempted.rows.is_empty() {
            continue;
        }
        let mut history: Vec<ProcedureRevision> = read(store, "MATCH (p:ProcedureRevision) WHERE p.project_id = $project AND p.anchor = $anchor AND p.ended_ms IS NULL RETURN p ORDER BY p.created_ms DESC LIMIT 3", json!({"project": project, "anchor": anchor}), 3).await?;
        for p in &mut history {
            p.uses = uses(store, &p.key).await?;
        }
        return Ok(ProcedureMaterial {
            episodes: group,
            history,
        });
    }
    Ok(ProcedureMaterial::default())
}

pub async fn propose(store: &GraphStore, p: ProcedureProposal) -> Result<(), RpcError> {
    let keys: BTreeSet<_> = p.episode_keys.iter().cloned().collect();
    if !(2..=8).contains(&keys.len()) {
        return Err(RpcError::invalid_params(
            "Use two to eight stored episodes from different tasks.",
        ));
    }
    let episodes: Vec<ProcedureEpisode> = read(
        store,
        "MATCH (e:ProcedureEpisode) WHERE e.key IN $keys AND e.ended_ms IS NULL RETURN e",
        json!({"keys": keys}),
        8,
    )
    .await?;
    if episodes.len() != keys.len() {
        return Err(RpcError::invalid_params(
            "Some contributing episodes are no longer available.",
        ));
    }
    let first = &episodes[0];
    if episodes
        .iter()
        .any(|e| e.project_id != first.project_id || e.anchor != first.anchor)
        || episodes
            .iter()
            .map(|e| (&e.session_id, &e.run_id))
            .collect::<BTreeSet<_>>()
            .len()
            < 2
        || episodes
            .iter()
            .map(|e| &e.task_key)
            .collect::<BTreeSet<_>>()
            .len()
            < 2
    {
        return Err(RpcError::invalid_params(
            "Contributing episodes must share a project and applicability anchor, across different tasks and runs.",
        ));
    }
    let attempt_key = material_key(&p.episode_keys);
    let previous_attempt = store
        .query(GraphQueryParams {
            cypher: "MATCH (a:ProcedureAttempt {key: $key}) WHERE a.ready = true RETURN a".into(),
            params: object(json!({"key": attempt_key})),
            limit: Some(1),
        })
        .await?;
    if !previous_attempt.rows.is_empty() {
        return Ok(());
    }
    let attempt = node(
        "ProcedureAttempt",
        &attempt_key,
        &json!({"project_id": first.project_id, "created_ms": now_ms(), "useful": p.useful}),
    )?;
    let mut edges: Vec<_> = episodes
        .iter()
        .map(|e| edge(&attempt, "USED", "ProcedureEpisode", &e.key))
        .collect();
    let mut nodes = Vec::new();
    if p.useful {
        let plain = |text: &str| {
            !text
                .chars()
                .any(|c| matches!(c, '\n' | '\r' | '\0' | '{' | '}'))
                && !text.contains("```")
        };
        if p.name.trim().is_empty()
            || p.name.chars().count() > 80
            || p.applicability.trim().is_empty()
            || p.applicability.chars().count() > 300
            || !plain(&p.name)
            || !plain(&p.applicability)
            || !(2..=5).contains(&p.steps.len())
            || p.steps
                .iter()
                .any(|s| s.trim().is_empty() || s.chars().count() > 240 || !plain(s))
        {
            return Err(RpcError::invalid_params(
                "A procedure needs a short name, applicability and two to five short steps.",
            ));
        }
        let family = hash(&format!(
            "{}\n{}\n{}",
            first.project_id,
            first.anchor,
            p.name.trim().to_lowercase()
        ));
        let normalized_steps: Vec<String> = p.steps.iter().map(|s| s.trim().to_owned()).collect();
        let key = hash(&format!(
            "{}\n{}\n{}",
            family,
            p.applicability.trim(),
            normalized_steps.join("\n")
        ));
        // A rejected exact revision is never resurrected by another synthesis.
        let existing: Vec<ProcedureRevision> = read(
            store,
            "MATCH (p:ProcedureRevision {key: $key}) RETURN p",
            json!({"key": key}),
            1,
        )
        .await?;
        if existing.first().is_none_or(|p| !p.ready) {
            let older: Vec<ProcedureRevision> = read(store, "MATCH (p:ProcedureRevision {family: $family}) WHERE p.ended_ms IS NULL RETURN p ORDER BY p.created_ms DESC LIMIT 1", json!({"family": family}), 1).await?;
            let item = ProcedureRevision {
                key: key.clone(),
                family,
                project_id: first.project_id.clone(),
                anchor: first.anchor.clone(),
                name: p.name.trim().into(),
                applicability: p.applicability.trim().into(),
                steps: normalized_steps,
                state: "candidate".into(),
                created_ms: now_ms(),
                ..Default::default()
            };
            let n = node("ProcedureRevision", &key, &item)?;
            edges.extend(
                episodes
                    .iter()
                    .map(|e| edge(&n, "DERIVED_FROM", "ProcedureEpisode", &e.key)),
            );
            if let Some(old) = older.first() {
                edges.push(edge(&n, "REVISES", "ProcedureRevision", &old.key));
            }
            nodes.push(n);
        }
    }
    // Attempt last: an interrupted put can be repeated without losing an unfinished revision.
    if !nodes.is_empty() {
        let key = nodes[0].key.clone();
        put(
            store,
            nodes,
            edges
                .iter()
                .filter(|e| e.from.label != "ProcedureAttempt")
                .cloned()
                .collect(),
        )
        .await?;
        put(
            store,
            vec![node("ProcedureRevision", &key, &json!({"ready": true}))?],
            Vec::new(),
        )
        .await?;
    }
    put(
        store,
        vec![attempt],
        edges
            .into_iter()
            .filter(|e| e.from.label == "ProcedureAttempt")
            .collect(),
    )
    .await?;
    put(
        store,
        vec![node(
            "ProcedureAttempt",
            &attempt_key,
            &json!({"ready": true}),
        )?],
        Vec::new(),
    )
    .await
}

pub async fn list(store: &GraphStore, project: &str) -> Result<ProcedureListResult, RpcError> {
    let mut items: Vec<ProcedureRevision> = read(store, "MATCH (p:ProcedureRevision) WHERE p.project_id = $project AND p.ready = true AND p.ended_ms IS NULL RETURN p ORDER BY p.created_ms DESC LIMIT 50", json!({"project": project}), 50).await?;
    if items.is_empty() {
        return Ok(ProcedureListResult { items });
    }
    let keys: Vec<String> = items.iter().map(|p| p.key.clone()).collect();
    let source_rows = store.query(GraphQueryParams {
        cypher: "MATCH (p:ProcedureRevision)-[:DERIVED_FROM]->(e:ProcedureEpisode) WHERE p.key IN $keys AND e.ended_ms IS NULL RETURN p.key, e LIMIT 400".into(),
        params: object(json!({"keys": keys})), limit: Some(400),
    }).await?;
    for row in source_rows.rows {
        if let Some(item) = items
            .iter_mut()
            .find(|p| Some(p.key.as_str()) == row.first().and_then(Value::as_str))
        {
            if let Some(props) = row.get(1).and_then(|v| v.get("properties")) {
                item.sources.push(
                    serde_json::from_value(props.clone())
                        .map_err(|e| RpcError::internal(e.to_string()))?,
                );
            }
        }
    }
    let use_rows = store.query(GraphQueryParams {
        cypher: "MATCH (u:ProcedureUse)-[:FOR]->(p:ProcedureRevision) WHERE p.key IN $keys AND u.ended_ms IS NULL RETURN p.key, u ORDER BY u.created_ms DESC LIMIT 2000".into(),
        params: object(json!({"keys": keys})), limit: Some(2000),
    }).await?;
    for row in use_rows.rows {
        if let Some(item) = items
            .iter_mut()
            .find(|p| Some(p.key.as_str()) == row.first().and_then(Value::as_str))
        {
            if let Some(props) = row.get(1).and_then(|v| v.get("properties")) {
                item.uses.push(
                    serde_json::from_value(props.clone())
                        .map_err(|e| RpcError::internal(e.to_string()))?,
                );
            }
        }
    }
    Ok(ProcedureListResult { items })
}

fn words(text: &str) -> BTreeSet<String> {
    const COMMON: &[&str] = &[
        "this", "that", "with", "when", "from", "have", "only", "will", "then", "into", "than",
        "your", "their", "there", "these", "those", "should", "would", "could", "before", "after",
        "about", "using", "does", "code", "task", "work",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 3)
        .map(str::to_lowercase)
        .filter(|s| !COMMON.contains(&s.as_str()))
        .collect()
}

pub async fn recall(
    store: &GraphStore,
    p: ProcedureRecallParams,
) -> Result<ProcedureListResult, RpcError> {
    if p.work.trim().is_empty()
        || p.work.len() > 16000
        || p.session_id.is_empty()
        || p.run_id.is_empty()
    {
        return Ok(ProcedureListResult::default());
    }
    let task = task_key(&p.work);
    let wanted = words(&p.work);
    let mut ranked = Vec::new();
    let eligible: Vec<ProcedureRevision> = read(store, "MATCH (p:ProcedureRevision) WHERE p.project_id = $project AND p.ready = true AND p.state IN ['trial', 'active'] AND p.ended_ms IS NULL RETURN p ORDER BY p.created_ms DESC LIMIT 50", json!({"project": p.project_id}), 50).await?;
    for item in eligible {
        if let Some(lang) = item.anchor.strip_prefix("lang:") {
            if !p.languages.iter().any(|l| l.eq_ignore_ascii_case(lang)) {
                continue;
            }
        } else {
            continue;
        }
        let score = wanted
            .intersection(&words(&format!("{} {}", item.name, item.applicability)))
            .count();
        if score >= 2 {
            ranked.push((score, item));
        }
    }
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.created_ms.cmp(&a.1.created_ms))
    });
    let mut chosen = None;
    for (_, mut item) in ranked.into_iter().take(3) {
        item.sources = sources(store, &item.key).await?;
        if item
            .sources
            .iter()
            .any(|e| e.task_key == task || (e.session_id == p.session_id && e.run_id == p.run_id))
        {
            continue;
        }
        chosen = Some(item);
        break;
    }
    let Some(mut item) = chosen else {
        return Ok(ProcedureListResult::default());
    };
    // Keyed by revision and run: retries cannot multiply exposure evidence.
    let key = hash(&format!("{}\n{}\n{}", item.key, p.session_id, p.run_id));
    let existing: Vec<ProcedureUse> = read(
        store,
        "MATCH (u:ProcedureUse {key: $key}) RETURN u",
        json!({"key": key}),
        1,
    )
    .await?;
    if existing.is_empty() {
        let use_ = ProcedureUse {
            key: key.clone(),
            revision: item.key.clone(),
            project_id: p.project_id,
            session_id: p.session_id,
            run_id: p.run_id,
            task_key: task,
            work: p.work.chars().take(500).collect(),
            outcome: "running".into(),
            shown: false,
            verdict: "unreviewed".into(),
            reason: String::new(),
            created_ms: now_ms(),
        };
        let n = node("ProcedureUse", &key, &use_)?;
        put(
            store,
            vec![n.clone()],
            vec![edge(&n, "FOR", "ProcedureRevision", &item.key)],
        )
        .await?;
    } else {
        let n = node("ProcedureUse", &key, &json!({}))?;
        put(
            store,
            vec![n.clone()],
            vec![edge(&n, "FOR", "ProcedureRevision", &item.key)],
        )
        .await?;
    }
    item.sources.clear();
    item.uses.clear();
    Ok(ProcedureListResult { items: vec![item] })
}

pub async fn outcome(store: &GraphStore, p: ProcedureOutcome) -> Result<(), RpcError> {
    if !matches!(
        p.outcome.as_str(),
        "done" | "failed" | "stopped" | "unknown"
    ) {
        return Err(RpcError::invalid_params("Unknown procedure-use outcome."));
    }
    let items: Vec<ProcedureUse> = read(store, "MATCH (u:ProcedureUse) WHERE u.project_id = $project AND u.session_id = $session AND u.run_id = $run AND u.ended_ms IS NULL RETURN u", json!({"project": p.project_id, "session": p.session_id, "run": p.run_id}), 10).await?;
    for mut item in items {
        if !p.revisions.contains(&item.revision) {
            continue;
        }
        item.outcome = p.outcome.clone();
        item.shown = true;
        let mut n = node("ProcedureUse", &item.key, &item)?;
        n.props.remove("verdict"); // Never overwrite an owner's review during a retry/backlog pass.
        n.props.remove("reason");
        put(store, vec![n], Vec::new()).await?;
    }
    Ok(())
}

pub async fn review(store: &GraphStore, p: ProcedureReview) -> Result<(), RpcError> {
    let item = revision(store, &p.revision, &p.project_id).await?;
    if !item.ready {
        return Err(RpcError::invalid_params(
            "This revision is still being recorded; refresh its evidence before reviewing.",
        ));
    }
    let mut new_state = None;
    match p.action.as_str() {
        "trial" if item.state == "candidate" => new_state = Some("trial"),
        "reject" if matches!(item.state.as_str(), "candidate" | "trial") => {
            new_state = Some("rejected")
        }
        "withdraw" => new_state = Some("withdrawn"),
        "approve" if item.state == "trial" => {
            let origins = sources(store, &item.key).await?;
            let observations = uses(store, &item.key).await?;
            let helpful: BTreeSet<_> = observations
                .iter()
                .filter(|u| {
                    u.shown
                        && u.verdict == "helped"
                        && u.outcome == "done"
                        && !origins.iter().any(|e| {
                            e.task_key == u.task_key
                                || (e.session_id == u.session_id && e.run_id == u.run_id)
                        })
                })
                .map(|u| u.task_key.clone())
                .collect();
            if helpful.len() < 2 || observations.iter().any(|u| u.verdict == "hurt") {
                return Err(RpcError::invalid_params(
                    "Review two completed trials on different non-origin tasks as helpful before approving regular use.",
                ));
            }
            // Only one approved revision of this method remains in regular use.
            let older: Vec<ProcedureRevision> = read(store, "MATCH (p:ProcedureRevision {family: $family}) WHERE p.state = 'active' AND p.key <> $key AND p.ended_ms IS NULL RETURN p", json!({"family": item.family, "key": item.key}), 50).await?;
            for old in older {
                change_state(
                    store,
                    &old.key,
                    "withdrawn",
                    "A newer owner-approved revision replaces this method.",
                )
                .await?;
            }
            new_state = Some("active");
        }
        "helped" | "hurt" | "unclear" => {
            let key = p.use_key.as_deref().ok_or_else(|| {
                RpcError::invalid_params("Choose the exact completed use to review.")
            })?;
            let observations = uses(store, &item.key).await?;
            let observed = observations
                .iter()
                .find(|u| u.key == key)
                .ok_or_else(|| RpcError::not_found("This use does not belong to this revision."))?;
            if !observed.shown
                || observed.outcome == "running"
                || (p.action == "helped" && observed.outcome != "done")
            {
                return Err(RpcError::invalid_params(
                    "Only a completed successful task can be marked helpful.",
                ));
            }
            let mut n = node(
                "ProcedureUse",
                key,
                &json!({"reviewed_ms": now_ms(), "reason": p.reason}),
            )?;
            n.temporal.insert("verdict".into(), json!(p.action));
            put(store, vec![n], Vec::new()).await?;
            if p.action == "hurt" {
                new_state = Some("withdrawn");
            } else if item.state == "active" {
                let origins = sources(store, &item.key).await?;
                let current = uses(store, &item.key).await?;
                let helpful: BTreeSet<_> = current
                    .iter()
                    .filter(|u| {
                        u.shown
                            && u.outcome == "done"
                            && u.verdict == "helped"
                            && !origins.iter().any(|e| {
                                e.task_key == u.task_key
                                    || (e.session_id == u.session_id && e.run_id == u.run_id)
                            })
                    })
                    .map(|u| &u.task_key)
                    .collect();
                if helpful.len() < 2 {
                    new_state = Some("trial");
                }
            }
        }
        _ => {
            return Err(RpcError::invalid_params(
                "This review action does not apply to the current revision.",
            ));
        }
    }
    if let Some(state) = new_state {
        change_state(store, &item.key, state, &p.reason).await?;
    }
    Ok(())
}

async fn change_state(
    store: &GraphStore,
    key: &str,
    state: &str,
    reason: &str,
) -> Result<(), RpcError> {
    let mut n = node(
        "ProcedureRevision",
        key,
        &json!({"reviewed_ms": now_ms(), "reason": reason.chars().take(1000).collect::<String>()}),
    )?;
    n.temporal.insert("state".into(), json!(state));
    put(store, vec![n], Vec::new()).await
}

/// The project owns this knowledge even though background records live in the app graph.
/// Retirement prevents a later reopening of the same path from reviving deleted knowledge.
pub async fn forget(store: &GraphStore, project: &str) -> Result<(), RpcError> {
    for label in [
        "ProcedureUse",
        "ProcedureRevision",
        "ProcedureAttempt",
        "ProcedureEpisode",
    ] {
        loop {
            let query = format!(
                "MATCH (p:{label}) WHERE p.project_id = $project AND p.ended_ms IS NULL RETURN p LIMIT 200"
            );
            let records: Vec<Value> = read(store, &query, json!({"project": project}), 200).await?;
            let refs: Vec<_> = records
                .iter()
                .filter_map(|p| p.get("key").and_then(Value::as_str))
                .map(|key| gref(label, key))
                .collect();
            if refs.is_empty() {
                break;
            }
            store
                .retire(GraphRetireParams {
                    refs,
                    reason: Some("Project deleted".into()),
                    at_ms: None,
                })
                .await?;
        }
    }
    Ok(())
}
