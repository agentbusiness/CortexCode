//! Work state in the WORKSPACE layer: tasks (hierarchy, contracts, dependencies, touched code),
//! the board, agents, rules (CORTEX.md and learned), user style and run records.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    AgentRecord, BoardListParams, contract_change, BoardMessage, BoardPostParams, Contract, ContractCheck, ContractRevision, Rule, RunRecordParams, StyleEntry, Task, TaskCreateParams,
    TaskUpdateParams,
};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The work store of one IronGraph project (a repository, or the global scope).
pub struct WorkStore {
    pub graph: Arc<Graph>,
    pub ig_id: String,
    /// Frame project id, or `global`.
    pub scope: String,
    indexes: Mutex<Vec<&'static str>>,
}

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

fn opt_s(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(|v| v.as_str()).map(str::to_owned)
}

fn n(row: &[Value], i: usize) -> u64 {
    row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0)
}

fn list(row: &[Value], i: usize) -> Vec<String> {
    row.get(i).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

const TASK_COLS: &str = "t.id, t.project_id, t.parent_id, t.title, t.brief, t.plan, t.status, t.assignee, t.depends_on, t.touches, t.result, t.session_id, t.created_ms, t.updated_ms, t.contract_hash, t.contract_revisions";

impl WorkStore {
    pub fn new(graph: Arc<Graph>, ig_id: String, scope: String) -> Self {
        Self { graph, ig_id, scope, indexes: Mutex::new(Vec::new()) }
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    async fn index(&self, name: &'static str, label: &str, property: &str) -> Result<(), RpcError> {
        let mut done = self.indexes.lock().await;
        if done.contains(&name) {
            return Ok(());
        }
        self.graph.ensure_index(&self.ig_id, name, label, property).await?;
        done.push(name);
        Ok(())
    }

    /// Link `from` (a work node matched by `match_clause`) to the code nodes named by `refs`
    /// (sids, repository paths, qualified or unique bare names).
    async fn link_code(&self, match_clause: &str, key: &Map<String, Value>, rel: &str, refs: &[String]) -> Result<(), RpcError> {
        for (position, reference) in refs.iter().enumerate() {
            let target = self.code_sid(reference).await?;
            let Some(target) = target else { continue };
            let mut p = key.clone();
            p.insert("target".into(), json!(target));
            p.insert("ref".into(), json!(reference));
            p.insert("position".into(), json!(position));
            self.run(&format!("{} {match_clause}, (c:Code {{sid: $target}}) CREATE (w)-[:{rel} {{ref: $ref, position: $position}}]->(c)", layer::WORKSPACE_WRITE), p).await?;
        }
        Ok(())
    }

    /// The sid of the code node a reference names, if exactly one.
    pub async fn code_sid(&self, reference: &str) -> Result<Option<String>, RpcError> {
        let attempts = [
            ("MATCH (c:Code {sid: $v}) RETURN c.sid", reference.to_owned()),
            ("MATCH (c:File {sid: $v}) RETURN c.sid", format!("file:{}", reference.trim_start_matches("./"))),
            ("MATCH (c:Sym {qname: $v}) RETURN c.sid", reference.to_owned()),
            ("MATCH (c:Sym {name: $v}) RETURN c.sid", reference.to_owned()),
        ];
        for (cypher, value) in attempts {
            let rows = self.run(&format!("{} {cypher}", layer::OBSERVED_READ), params! { "v" => value }).await?;
            if rows.rows.len() == 1 {
                return Ok(Some(s(&rows.rows[0], 0)));
            }
            if rows.rows.len() > 1 {
                return Ok(None);
            }
        }
        Ok(None)
    }

    // ---- tasks ---------------------------------------------------------------------------------

    pub async fn task_create(&self, p: TaskCreateParams) -> Result<Task, RpcError> {
        let id = new_id("task");
        let now = now_ms();
        if let Some(parent) = &p.parent_id
            && self.task_row(parent).await?.is_none()
        {
            return Err(RpcError::not_found(format!("parent task {parent} not found")));
        }
        self.run(
            &format!(
                "{} CREATE (t:Task {{id: $id, name: $title, project_id: $project, parent_id: $parent, title: $title, brief: $brief, plan: $plan, status: 'pending', assignee: $assignee, depends_on: $depends, touches: $touches, created_ms: $now, updated_ms: $now}})",
                layer::WORKSPACE_WRITE
            ),
            params! {
                "id" => id, "project" => self.scope, "parent" => p.parent_id, "title" => p.title, "brief" => p.brief, "plan" => p.plan,
                "assignee" => p.assignee, "depends" => p.depends_on, "touches" => p.touches, "now" => now
            },
        )
        .await?;
        self.index("task_id", "Task", "id").await?;
        let key = params! { "id" => id };
        if let Some(parent) = &p.parent_id {
            self.run(&format!("{} MATCH (t:Task {{id: $id}}), (p:Task {{id: $parent}}) CREATE (p)-[:HAS_SUBTASK]->(t)", layer::WORKSPACE_WRITE), params! { "id" => id, "parent" => parent }).await?;
        }
        for dep in &p.depends_on {
            self.run(&format!("{} MATCH (t:Task {{id: $id}}), (d:Task {{id: $dep}}) CREATE (t)-[:DEPENDS_ON]->(d)", layer::WORKSPACE_WRITE), params! { "id" => id, "dep" => dep }).await?;
        }
        self.write_contract(&id, &p.contract).await?;
        self.link_code("MATCH (w:Task {id: $id})", &key, "TOUCHES", &p.touches).await?;
        self.task_get(&id).await?.ok_or_else(|| RpcError::internal("task vanished after creation"))
    }

    async fn write_contract(&self, id: &str, contract: &Contract) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (t:Task {{id: $id}})-[:HAS_CHECK]->(c:ContractCheck) DETACH DELETE c", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        if contract.checks.is_empty() {
            return Ok(());
        }
        let rows: Vec<Value> = contract
            .checks
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"name": c.name, "kind": c.kind, "spec": c.spec, "expect": c.expect, "position": i}))
            .collect();
        self.run(
            &format!(
                "{} MATCH (t:Task {{id: $id}}) UNWIND $rows AS r CREATE (t)-[:HAS_CHECK {{position: r.position}}]->(c:ContractCheck {{name: r.name, kind: r.kind, spec: r.spec, expect: r.expect, position: r.position, task_id: $id}})",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "rows" => rows },
        )
        .await?;
        Ok(())
    }

    async fn task_row(&self, id: &str) -> Result<Option<Vec<Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (t:Task {{id: $id}}) RETURN {TASK_COLS}", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        Ok(rows.rows.into_iter().next())
    }

    fn task_from(row: &[Value]) -> Task {
        Task {
            id: s(row, 0),
            project_id: s(row, 1),
            parent_id: opt_s(row, 2),
            title: s(row, 3),
            brief: s(row, 4),
            plan: s(row, 5),
            contract: Contract::default(),
            status: s(row, 6),
            assignee: opt_s(row, 7),
            depends_on: list(row, 8),
            touches: list(row, 9),
            result: opt_s(row, 10),
            session_id: opt_s(row, 11),
            created_ms: n(row, 12),
            updated_ms: n(row, 13),
            contract_hash: opt_s(row, 14),
            // The lineage is `ContractRevision` nodes (filled in by `complete`); an earlier version
            // kept it as JSON text, read here until the task's next revision rewrites it.
            contract_revisions: opt_s(row, 15).and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default(),
        }
    }

    /// Contract checks and current touch targets for a set of tasks.
    async fn complete(&self, tasks: &mut [Task]) -> Result<(), RpcError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        let checks = self
            .run(
                &format!("{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[:HAS_CHECK]->(c:ContractCheck) RETURN t.id, c.position, c.name, c.kind, c.spec, c.expect", layer::WORKSPACE_READ),
                params! { "ids" => ids },
            )
            .await?;
        let mut by_task: HashMap<String, Vec<(u64, ContractCheck)>> = HashMap::new();
        for row in &checks.rows {
            by_task.entry(s(row, 0)).or_default().push((n(row, 1), ContractCheck { name: s(row, 2), kind: s(row, 3), spec: s(row, 4), expect: opt_s(row, 5) }));
        }
        // Touch links follow their code node through edits and renames: report the node's current sid.
        let touches = self
            .run(
                &format!("{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[r:TOUCHES]->(c:Code) RETURN t.id, r.ref, r.position, c.sid, c.kind, c.path", layer::ALL_READ),
                params! { "ids" => ids },
            )
            .await?;
        let revisions = self
            .run(&format!("{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[:REVISED]->(r:ContractRevision) RETURN i, r", layer::WORKSPACE_READ), params! { "ids" => ids })
            .await?;
        let mut lineage: HashMap<String, Vec<(u64, ContractRevision)>> = HashMap::new();
        for row in &revisions.rows {
            let Some(props) = row.get(1).and_then(|node| node.get("properties")).cloned() else { continue };
            let position = props.get("position").and_then(Value::as_u64).unwrap_or(0);
            if let Ok(revision) = serde_json::from_value::<ContractRevision>(props) {
                lineage.entry(s(row, 0)).or_default().push((position, revision));
            }
        }
        let mut touch_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        for row in &touches.rows {
            let reference = s(row, 1);
            let current = if s(row, 4) == "file" { s(row, 5) } else { s(row, 3) };
            touch_map.entry(s(row, 0)).or_default().insert(reference, current);
        }
        for task in tasks.iter_mut() {
            if let Some(mut revisions) = lineage.remove(&task.id) {
                revisions.sort_by_key(|(position, _)| *position);
                task.contract_revisions = revisions.into_iter().map(|(_, revision)| revision).collect();
            }
            if let Some(mut checks) = by_task.remove(&task.id) {
                checks.sort_by_key(|(p, _)| *p);
                task.contract = Contract { checks: checks.into_iter().map(|(_, c)| c).collect() };
            }
            if let Some(map) = touch_map.get(&task.id) {
                for touch in task.touches.iter_mut() {
                    if let Some(current) = map.get(touch) {
                        *touch = current.clone();
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn task_get(&self, id: &str) -> Result<Option<Task>, RpcError> {
        let Some(row) = self.task_row(id).await? else { return Ok(None) };
        let mut tasks = vec![Self::task_from(&row)];
        self.complete(&mut tasks).await?;
        Ok(tasks.pop())
    }

    pub async fn task_update(&self, p: TaskUpdateParams) -> Result<Option<Task>, RpcError> {
        let Some(current) = self.task_get(&p.id).await? else { return Ok(None) };
        // A frozen contract changes only through an accepted revision (the shared rule).
        let change = contract_change(&current, &p, now_ms()).map_err(RpcError::invalid_params)?;
        let mut sets: Vec<String> = vec!["t.updated_ms = $now".into()];
        let mut values = params! { "id" => p.id, "now" => now_ms() };
        let mut set = |field: &str, value: Option<Value>| {
            if let Some(v) = value {
                sets.push(format!("t.{field} = ${field}"));
                values.insert(field.to_owned(), v);
            }
        };
        set("title", p.title.clone().map(Value::from));
        set("name", p.title.clone().map(Value::from));
        set("brief", p.brief.map(Value::from));
        set("plan", p.plan.map(Value::from));
        set("status", p.status.map(Value::from));
        set("assignee", p.assignee.map(Value::from));
        set("result", p.result.map(Value::from));
        set("session_id", p.session_id.map(Value::from));
        set("touches", p.touches.clone().map(|t| json!(t)));
        set("contract_hash", change.hash.clone().map(Value::from));
        if change.revision.is_some() {
            sets.push("t.contract_revisions = null".into());
        }
        self.run(&format!("{} MATCH (t:Task {{id: $id}}) SET {}", layer::WORKSPACE_WRITE, sets.join(", ")), values).await?;
        if let Some(revision) = &change.revision {
            // The lineage so far (an earlier version's text included) as nodes, then the new one.
            let mut lineage = current.contract_revisions.clone();
            lineage.push(revision.clone());
            for (position, revision) in lineage.iter().enumerate() {
                let mut props = match serde_json::to_value(revision) {
                    Ok(Value::Object(map)) => map,
                    _ => continue,
                };
                props.insert("id".into(), json!(format!("{}#{position}", p.id)));
                props.insert("task_id".into(), json!(p.id));
                props.insert("position".into(), json!(position));
                props.insert("first_ms".into(), json!(revision.ts_ms));
                props.insert("last_ms".into(), json!(revision.ts_ms));
                self.run(
                    &format!("{} MATCH (t:Task {{id: $task}}) MERGE (r:ContractRevision {{id: $rid}}) SET r += $props MERGE (t)-[:REVISED]->(r)", layer::WORKSPACE_WRITE),
                    params! { "task" => p.id, "rid" => format!("{}#{position}", p.id), "props" => props },
                )
                .await?;
            }
        }
        if let Some(contract) = &change.contract {
            self.write_contract(&p.id, contract).await?;
        }
        if let Some(touches) = &p.touches {
            self.run(&format!("{} MATCH (w:Task {{id: $id}})-[r:TOUCHES]->(c:Code) DELETE r", layer::WORKSPACE_WRITE), params! { "id" => p.id }).await?;
            self.link_code("MATCH (w:Task {id: $id})", &params! { "id" => p.id }, "TOUCHES", touches).await?;
        }
        self.task_get(&p.id).await
    }

    pub async fn task_list(&self, status: Option<&str>, parent: Option<&str>) -> Result<Vec<Task>, RpcError> {
        let rows = self.run(&format!("{} MATCH (t:Task) RETURN {TASK_COLS}", layer::WORKSPACE_READ), Map::new()).await?;
        let mut tasks: Vec<Task> = rows
            .rows
            .iter()
            .map(|r| Self::task_from(r))
            .filter(|t| status.is_none_or(|s| t.status == s) && parent.is_none_or(|p| t.parent_id.as_deref() == Some(p)))
            .collect();
        tasks.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        self.complete(&mut tasks).await?;
        Ok(tasks)
    }

    // ---- board ---------------------------------------------------------------------------------

    pub async fn board_post(&self, p: BoardPostParams) -> Result<BoardMessage, RpcError> {
        let message = BoardMessage { id: new_id("msg"), project_id: self.scope.clone(), kind: p.kind, author: p.author, text: p.text, refs: p.refs, to: p.to, ts_ms: now_ms() };
        self.run(
            &format!(
                "{} CREATE (m:BoardMessage {{id: $id, name: $kind, project_id: $project, kind: $kind, author: $author, text: $text, refs: $refs, to: $to, ts_ms: $ts}})",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => message.id, "project" => message.project_id, "kind" => message.kind, "author" => message.author, "text" => message.text, "refs" => message.refs, "to" => message.to, "ts" => message.ts_ms },
        )
        .await?;
        self.index("board_id", "BoardMessage", "id").await?;
        // References to code and tasks become edges.
        for reference in &message.refs {
            if self.task_row(reference).await?.is_some() {
                self.run(&format!("{} MATCH (m:BoardMessage {{id: $id}}), (t:Task {{id: $ref}}) CREATE (m)-[:REFERS_TO]->(t)", layer::WORKSPACE_WRITE), params! { "id" => message.id, "ref" => reference }).await?;
            }
        }
        let code_refs: Vec<String> = message.refs.clone();
        self.link_code("MATCH (w:BoardMessage {id: $id})", &params! { "id" => message.id }, "REFERS_TO", &code_refs).await?;
        Ok(message)
    }

    pub async fn board_list(&self, p: &BoardListParams) -> Result<Vec<BoardMessage>, RpcError> {
        let rows = self.run(&format!("{} MATCH (m:BoardMessage) RETURN m.id, m.project_id, m.kind, m.author, m.text, m.refs, m.to, m.ts_ms", layer::WORKSPACE_READ), Map::new()).await?;
        let mut messages: Vec<BoardMessage> = rows
            .rows
            .iter()
            .map(|r| BoardMessage { id: s(r, 0), project_id: s(r, 1), kind: s(r, 2), author: s(r, 3), text: s(r, 4), refs: list(r, 5), to: opt_s(r, 6), ts_ms: n(r, 7) })
            .filter(|m| p.since_ms.is_none_or(|since| m.ts_ms >= since))
            // Addressed to the reader, or to everyone.
            .filter(|m| p.to.as_ref().is_none_or(|to| m.to.is_none() || m.to.as_ref() == Some(to)))
            .collect();
        messages.sort_by(|a, b| (a.ts_ms, &a.id).cmp(&(b.ts_ms, &b.id)));
        Ok(messages)
    }

    // ---- agents --------------------------------------------------------------------------------

    pub async fn agent_upsert(&self, mut record: AgentRecord) -> Result<AgentRecord, RpcError> {
        record.project_id = self.scope.clone();
        if record.updated_ms == 0 {
            record.updated_ms = now_ms();
        }
        self.run(
            &format!(
                "{} MERGE (a:Agent {{name: $name}}) SET a.project_id = $project, a.role = $role, a.charter = $charter, a.status = $status, a.session_id = $session, a.task_id = $task, a.workspace = $workspace, a.tasks_done = $done, a.tasks_failed = $failed, a.updated_ms = $updated, a.mode = $mode",
                layer::WORKSPACE_WRITE
            ),
            params! {
                "name" => record.name, "project" => record.project_id, "role" => record.role, "charter" => record.charter, "status" => record.status,
                "session" => record.session_id, "task" => record.task_id, "workspace" => record.workspace, "done" => record.tasks_done,
                "failed" => record.tasks_failed, "updated" => record.updated_ms, "mode" => record.mode.clone()
            },
        )
        .await?;
        self.index("agent_name", "Agent", "name").await?;
        self.run(&format!("{} MATCH (a:Agent {{name: $name}})-[r:WORKS_ON]->() DELETE r", layer::WORKSPACE_WRITE), params! { "name" => record.name }).await?;
        if let Some(task) = &record.task_id {
            self.run(&format!("{} MATCH (a:Agent {{name: $name}}), (t:Task {{id: $task}}) CREATE (a)-[:WORKS_ON]->(t)", layer::WORKSPACE_WRITE), params! { "name" => record.name, "task" => task }).await?;
        }
        Ok(record)
    }

    pub async fn agent_list(&self) -> Result<Vec<AgentRecord>, RpcError> {
        let rows = self
            .run(
                &format!("{} MATCH (a:Agent) RETURN a.name, a.project_id, a.role, a.charter, a.status, a.session_id, a.task_id, a.workspace, a.tasks_done, a.tasks_failed, a.updated_ms, a.mode", layer::WORKSPACE_READ),
                Map::new(),
            )
            .await?;
        let mut agents: Vec<AgentRecord> = rows
            .rows
            .iter()
            .map(|r| AgentRecord {
                name: s(r, 0),
                project_id: s(r, 1),
                role: s(r, 2),
                charter: s(r, 3),
                status: s(r, 4),
                session_id: opt_s(r, 5),
                task_id: opt_s(r, 6),
                workspace: opt_s(r, 7),
                tasks_done: n(r, 8) as u32,
                tasks_failed: n(r, 9) as u32,
                updated_ms: n(r, 10),
                mode: opt_s(r, 11).filter(|mode| !mode.is_empty()).unwrap_or_else(|| "auto".into()),
            })
            .collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(agents)
    }

    // ---- rules ---------------------------------------------------------------------------------

    pub async fn rule_upsert(&self, mut rule: Rule) -> Result<Rule, RpcError> {
        if rule.id.is_empty() {
            rule.id = new_id("rule");
        }
        rule.scope = self.scope.clone();
        rule.updated_ms = now_ms();
        let name: String = rule.text.chars().take(80).collect();
        self.run(
            &format!(
                "{} MERGE (r:Rule {{id: $id}}) SET r.name = $name, r.scope = $scope, r.text = $text, r.source = $source, r.applies_to = $applies, r.updated_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => rule.id, "name" => name, "scope" => rule.scope, "text" => rule.text, "source" => rule.source, "applies" => rule.applies_to, "now" => rule.updated_ms },
        )
        .await?;
        self.index("rule_id", "Rule", "id").await?;
        self.run(&format!("{} MATCH (w:Rule {{id: $id}})-[c:CONSTRAINS]->() DELETE c", layer::WORKSPACE_WRITE), params! { "id" => rule.id }).await?;
        self.link_code("MATCH (w:Rule {id: $id})", &params! { "id" => rule.id }, "CONSTRAINS", &rule.applies_to).await?;
        Ok(rule)
    }

    pub async fn rule_list(&self) -> Result<Vec<Rule>, RpcError> {
        let rows = self.run(&format!("{} MATCH (r:Rule) RETURN r.id, r.scope, r.text, r.source, r.applies_to, r.updated_ms, r.position", layer::WORKSPACE_READ), Map::new()).await?;
        let mut rules: Vec<(u64, Rule)> = rows
            .rows
            .iter()
            .map(|r| (n(r, 6), Rule { id: s(r, 0), scope: s(r, 1), text: s(r, 2), source: s(r, 3), applies_to: list(r, 4), updated_ms: n(r, 5) }))
            .collect();
        rules.sort_by(|a, b| (a.1.source != "cortex_md", a.0, &a.1.id).cmp(&(b.1.source != "cortex_md", b.0, &b.1.id)));
        Ok(rules.into_iter().map(|(_, r)| r).collect())
    }

    /// Mirror a CORTEX.md into `cortex_md` rules: one rule per list item or paragraph, in order.
    /// Rules that are no longer in the file are removed. Returns the number of rules.
    pub async fn import_cortex_md(&self, text: &str) -> Result<usize, RpcError> {
        let items = parse_cortex_md(text);
        let now = now_ms();
        let mut ids = Vec::new();
        for (position, (section, item)) in items.iter().enumerate() {
            let id = format!("md-{}", &crate::facts::hash_hex(format!("{}\u{0}{section}\u{0}{item}", self.scope).as_bytes())[..20]);
            let name: String = item.chars().take(80).collect();
            self.run(
                &format!(
                    "{} MERGE (r:Rule {{id: $id}}) SET r.name = $name, r.scope = $scope, r.text = $text, r.source = 'cortex_md', r.section = $section, r.applies_to = [], r.position = $position, r.updated_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => id, "name" => name, "scope" => self.scope, "text" => item, "section" => section, "position" => position, "now" => now },
            )
            .await?;
            ids.push(id);
        }
        if !items.is_empty() {
            self.index("rule_id", "Rule", "id").await?;
        }
        let existing = self.run(&format!("{} MATCH (r:Rule {{source: 'cortex_md'}}) RETURN r.id", layer::WORKSPACE_READ), Map::new()).await?;
        let stale: Vec<String> = existing.column(0).filter_map(|v| v.as_str()).filter(|id| !ids.iter().any(|x| x == id)).map(str::to_owned).collect();
        if !stale.is_empty() {
            self.run(&format!("{} UNWIND $ids AS i MATCH (r:Rule {{id: i}}) DETACH DELETE r", layer::WORKSPACE_WRITE), params! { "ids" => stale }).await?;
        }
        Ok(items.len())
    }

    // ---- style ---------------------------------------------------------------------------------

    pub async fn style_upsert(&self, mut entry: StyleEntry) -> Result<StyleEntry, RpcError> {
        if entry.id.is_empty() {
            entry.id = new_id("style");
        }
        entry.scope = self.scope.clone();
        entry.updated_ms = now_ms();
        self.run(
            &format!(
                "{} MERGE (s:Style {{id: $id}}) SET s.name = $area, s.area = $area, s.text = $text, s.scope = $scope, s.confidence = $confidence, s.evidence = $evidence, s.updated_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => entry.id, "area" => entry.area, "text" => entry.text, "scope" => entry.scope, "confidence" => entry.confidence, "evidence" => entry.evidence, "now" => entry.updated_ms },
        )
        .await?;
        self.index("style_id", "Style", "id").await?;
        Ok(entry)
    }

    pub async fn style_list(&self) -> Result<Vec<StyleEntry>, RpcError> {
        let rows = self.run(&format!("{} MATCH (s:Style) RETURN s.id, s.area, s.text, s.scope, s.confidence, s.evidence, s.updated_ms", layer::WORKSPACE_READ), Map::new()).await?;
        let mut entries: Vec<StyleEntry> = rows
            .rows
            .iter()
            .map(|r| StyleEntry {
                id: s(r, 0),
                area: s(r, 1),
                text: s(r, 2),
                scope: s(r, 3),
                confidence: r.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                evidence: list(r, 5),
                updated_ms: n(r, 6),
            })
            .collect();
        entries.sort_by(|a, b| (&a.area, &a.id).cmp(&(&b.area, &b.id)));
        Ok(entries)
    }

    // ---- runs ----------------------------------------------------------------------------------

    pub async fn run_record(&self, p: RunRecordParams) -> Result<String, RpcError> {
        let id = new_id("run");
        let now = now_ms();
        self.run(
            &format!(
                "{} CREATE (r:Run {{id: $id, name: $outcome, project_id: $project, session_id: $session, task_id: $task, outcome: $outcome, summary: $summary, touched: $touched, ts_ms: $now, first_ms: $now, last_ms: $now}})",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "project" => self.scope, "session" => p.session_id, "task" => p.task_id, "outcome" => p.outcome, "summary" => p.summary, "touched" => p.touched, "now" => now },
        )
        .await?;
        // The run's measures are its own properties (`turns`, `tool_calls`, …): scalars only.
        let metrics: Map<String, Value> = p
            .metrics
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(name, value)| !value.is_object() && !value.is_array() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                    .map(|(name, value)| (format!("metric_{name}"), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if !metrics.is_empty() {
            self.run(&format!("{} MATCH (r:Run {{id: $id}}) SET r += $metrics", layer::WORKSPACE_WRITE), params! { "id" => id, "metrics" => metrics }).await?;
        }
        self.index("run_id", "Run", "id").await?;
        if let Some(task) = &p.task_id {
            self.run(&format!("{} MATCH (r:Run {{id: $id}}), (t:Task {{id: $task}}) CREATE (r)-[:FOR_TASK]->(t)", layer::WORKSPACE_WRITE), params! { "id" => id, "task" => task }).await?;
        }
        for reference in &p.touched {
            if let Some(target) = self.code_sid(reference).await? {
                self.run(&format!("{} MATCH (r:Run {{id: $id}}), (c:Code {{sid: $target}}) CREATE (c)-[:MODIFIED_IN]->(r)", layer::WORKSPACE_WRITE), params! { "id" => id, "target" => target }).await?;
            }
        }
        Ok(id)
    }
}

/// CORTEX.md structure (markdown syntax only): headings name sections; every list item (with its
/// indented continuation lines) and every plain paragraph is one rule. Fenced code is skipped.
pub fn parse_cortex_md(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut section = String::new();
    let mut current: Option<String> = None;
    let mut in_fence = false;
    let flush = |current: &mut Option<String>, out: &mut Vec<(String, String)>, section: &str| {
        if let Some(item) = current.take() {
            let item = item.trim().to_owned();
            if !item.is_empty() {
                out.push((section.to_owned(), item));
            }
        }
    };
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut current, &mut out, &section);
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut current, &mut out, &section);
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix('#') {
            flush(&mut current, &mut out, &section);
            section = heading.trim_start_matches('#').trim().to_owned();
            continue;
        }
        let bullet = ["- ", "* ", "+ "].iter().find_map(|b| trimmed.strip_prefix(b)).or_else(|| {
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() { trimmed[digits.len()..].strip_prefix(". ").or_else(|| trimmed[digits.len()..].strip_prefix(") ")) } else { None }
        });
        // A bullet at the item's own indentation starts a new rule; deeper bullets continue it.
        let indent = line.len() - trimmed.len();
        match bullet {
            Some(body) if indent < 2 || current.is_none() => {
                flush(&mut current, &mut out, &section);
                current = Some(body.trim().to_owned());
            }
            _ => {
                let piece = bullet.unwrap_or(trimmed);
                match &mut current {
                    Some(c) => {
                        c.push(' ');
                        c.push_str(piece.trim());
                    }
                    None => current = Some(piece.trim().to_owned()),
                }
            }
        }
    }
    flush(&mut current, &mut out, &section);
    out
}
