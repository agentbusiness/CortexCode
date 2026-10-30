//! The night report: once a day, when the learner has nothing left to do, a page for the owner
//! (a `NightReport` node, its text Markdown) on how the lessons fare, and which ones
//! earned a stronger form. A lesson moves up a ladder; nothing moves without the owner's word: the
//! report proposes, the owner applies the change (a skill section or a host repair is a change of
//! the repository).
//!
//! 1. a lesson shown with the knowledge note (found by the turn's topic);
//! 2. an error hint: a lesson with a failure key, shown right under that failure (automatic for
//!    lessons learned from a refusal or a failing command);
//! 3. a section in a skill: an error hint or lesson that helped at least `SKILL_HELPED` times and
//!    never hurt; INV (`tool:`, `approach`) to the tools guide or an engineering skill, DOM
//!    (`lang:`) to the language plugin's skill;
//! 4. a host repair: a refusal's lesson (`refused <tool> <kind>`) that helped at least
//!    `REPAIR_HELPED` times: the tools plugin could carry out the call its lesson describes.
//!
//! Also listed: the replays since the last report (`Replay` nodes, `replay.rs`) and the lessons
//! close to withdrawal (they hurt at least as often as they helped).
//!
//! ```text
//! (:NightReport {key: <date>, text, created_ms, lessons, keyed, repair, skill, replays, failing, topic_only})-[:ON]->(:Machine)
//! ```
//!
//! Whether today's report exists and when the last one was made are read from the graph. Older
//! versions wrote `reports/night-<date>.md` and `reports/last.json`; [`migrate`] moves them in once.

use crate::graph;
use crate::host::Host;
use crate::replay::{Paths, now_ms};
use cc_protocol::contracts::memory::{KnowledgeItem, KnowledgeListResult, knowledge, methods as memory_methods};
use serde_json::{Value, json};

const SKILL_HELPED: u32 = 3;
const REPAIR_HELPED: u32 = 5;

/// Today's date (`YYYY-MM-DD`, UTC).
fn today() -> String {
    let days = (now_ms() / 86_400_000) as i64;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn anchor(item: &KnowledgeItem) -> &str {
    item.name.split(": ").next().unwrap_or("")
}

fn kind(item: &KnowledgeItem) -> &'static str {
    if anchor(item).starts_with("lang:") { "DOM" } else { "INV" }
}

fn key(item: &KnowledgeItem) -> Option<&str> {
    item.aliases.iter().map(String::as_str).find(|alias| alias.starts_with("refused ") || alias.starts_with("failed "))
}

fn line(item: &KnowledgeItem) -> String {
    let evidence = item.evidence.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    format!("- [{}] `{}`: {} (helped {}, hurt {}, seen {}; {})", kind(item), anchor(item), item.text.trim(), item.helped, item.hurt, item.seen, evidence)
}

/// Where a lesson's skill section would go.
fn skill_home(item: &KnowledgeItem) -> String {
    match anchor(item).strip_prefix("lang:") {
        Some(language) => format!("the `lang-{language}` plugin's skill"),
        None => "the tools guide (`plugins/tools/backend/src/schemas.rs`) or an engineering skill".to_owned(),
    }
}

const LABEL: &str = "NightReport";

/// Write today's report unless it exists. Returns its date when written.
pub async fn report_if_due(host: &Host) -> Option<String> {
    let date = today();
    let made = graph::rows(host, "MATCH (r:NightReport {key: $key}) RETURN r.key", json!({"key": date}), 1).await.ok()?;
    if !made.is_empty() {
        return None;
    }
    let listed = host.get::<KnowledgeListResult>(memory_methods::KNOWLEDGE_LIST, json!({"kind": knowledge::LESSON})).await.ok()?;
    let lessons = listed.items;
    let last = graph::rows(host, "MATCH (r:NightReport) RETURN max(r.created_ms)", json!({}), 1).await.ok()?;
    let since = last.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0);
    let replays = graph::nodes(host, "MATCH (r:Replay) WHERE r.ts_ms > $since RETURN r ORDER BY r.ts_ms", json!({"since": since}), 5000).await.ok()?;
    let mut out: Vec<String> = vec![format!("# Night report {}", today()), String::new()];
    out.push(format!("{} live lessons; {} with a failure key (shown under their failure).", lessons.len(), lessons.iter().filter(|item| key(item).is_some()).count()));
    out.push(String::new());
    out.push("Each proposal waits for your word: nothing here is applied by itself.".to_owned());
    out.push(String::new());

    let repair: Vec<&KnowledgeItem> = lessons.iter().filter(|item| key(item).is_some_and(|k| k.starts_with("refused ")) && item.helped >= REPAIR_HELPED && item.hurt == 0).collect();
    out.push("## Proposed: a host repair".to_owned());
    out.push(String::new());
    if repair.is_empty() {
        out.push("None.".to_owned());
    }
    for item in &repair {
        out.push(line(item));
        out.push(format!("  The tools plugin (`plugins/tools/backend/src/repair/`) could carry out what this says for `{}`, with a note.", key(item).unwrap_or("")));
    }
    out.push(String::new());

    let skill: Vec<&KnowledgeItem> = lessons.iter().filter(|item| item.helped >= SKILL_HELPED && item.hurt == 0 && !repair.iter().any(|r| r.id == item.id)).collect();
    out.push("## Proposed: a section in a skill".to_owned());
    out.push(String::new());
    if skill.is_empty() {
        out.push("None.".to_owned());
    }
    for item in &skill {
        out.push(line(item));
        out.push(format!("  Into {}.", skill_home(item)));
    }
    out.push(String::new());

    out.push("## Replays since the last report".to_owned());
    out.push(String::new());
    if replays.is_empty() {
        out.push("None.".to_owned());
    }
    for row in &replays {
        let field = |name: &str| graph::str_prop(row, name).to_owned();
        let dom = if row.get("dom").and_then(Value::as_bool).unwrap_or(false) { "DOM" } else { "INV" };
        out.push(format!("- [{dom}] {}: with it {}, without it {}: {} ({}; learned at {})", field("text"), field("with_state"), field("without_state"), field("verdict").replace('_', " "), field("evidence"), field("of")));
    }
    out.push(String::new());

    let failing: Vec<&KnowledgeItem> = lessons.iter().filter(|item| item.hurt > 0 && item.hurt >= item.helped).collect();
    out.push("## Close to withdrawal (hurt at least as often as helped)".to_owned());
    out.push(String::new());
    if failing.is_empty() {
        out.push("None.".to_owned());
    }
    for item in &failing {
        out.push(line(item));
    }
    out.push(String::new());

    let topic_only = lessons.iter().filter(|item| key(item).is_none()).count();
    out.push(format!("{topic_only} lessons have no failure key (learned from a steer or a retry, or before failure keys): only a turn's topic finds them."));
    out.push(String::new());

    let counts = json!({
        "text": out.join("\n"), "created_ms": now_ms(), "lessons": lessons.len(), "keyed": lessons.iter().filter(|item| key(item).is_some()).count(),
        "repair": repair.len(), "skill": skill.len(), "replays": replays.len(), "failing": failing.len(), "topic_only": topic_only,
    });
    if let Err(error) = graph::put_on_machine(host, graph::node(LABEL, &date, graph::props(counts)), None).await {
        tracing::warn!(error = %error.message, "the night report was not recorded; tried again when the learner is next idle");
        return None;
    }
    tracing::info!(%date, "wrote the night report");
    Some(date)
}

/// Move the reports older versions wrote (`reports/night-<date>.md`, `reports/last.json`) into
/// the graph, once. The last report's time comes from `last.json`, the others' from their files.
pub async fn migrate(host: &Host, paths: &Paths) {
    let last = paths.reports.join("last.json");
    let marked = std::fs::read_to_string(&last).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let marked_file = marked.as_ref().and_then(|value| value.get("file").and_then(Value::as_str)).map(std::path::PathBuf::from);
    let marked_ms = marked.as_ref().and_then(|value| value.get("ts_ms").and_then(Value::as_u64));
    let mut moved = 0;
    for entry in std::fs::read_dir(&paths.reports).into_iter().flatten().flatten() {
        let file = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(date) = name.strip_prefix("night-").and_then(|rest| rest.strip_suffix(".md")) else { continue };
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let modified = entry.metadata().and_then(|meta| meta.modified()).ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_millis() as u64);
        let created_ms = if marked_file.as_ref() == Some(&file) { marked_ms.or(modified) } else { modified }.unwrap_or_else(now_ms).min(now_ms());
        let node = graph::node(LABEL, date, graph::props(json!({"text": text, "created_ms": created_ms})));
        if let Err(error) = graph::put_on_machine(host, node, Some(created_ms)).await {
            tracing::warn!(error = %error.message, "the night reports were not moved into the graph; tried again at the next start");
            return;
        }
        let _ = std::fs::remove_file(&file);
        moved += 1;
    }
    let _ = std::fs::remove_file(&last);
    let _ = std::fs::remove_dir(&paths.reports);
    if moved > 0 {
        tracing::info!(moved, "moved the night reports into the graph");
    }
}
