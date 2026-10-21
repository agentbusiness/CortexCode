//! The night report: once a day, when the learner has nothing left to do, a page for the owner in
//! the plugin's data folder (`reports/night-<date>.md`) on how the lessons fare, and which ones
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
//! Also listed: the replays since the last report (`replay.rs`) and the lessons close to
//! withdrawal (they hurt at least as often as they helped).

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

/// Write today's report unless it exists. Returns its path when written.
pub async fn report_if_due(host: &Host, paths: &Paths) -> Option<std::path::PathBuf> {
    let file = paths.reports.join(format!("night-{}.md", today()));
    if file.exists() {
        return None;
    }
    let listed = host.get::<KnowledgeListResult>(memory_methods::KNOWLEDGE_LIST, json!({"kind": knowledge::LESSON})).await.ok()?;
    let lessons = listed.items;
    let since = std::fs::read_to_string(paths.reports.join("last.json")).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()).and_then(|value| value.get("ts_ms").and_then(Value::as_u64)).unwrap_or(0);
    let replays: Vec<Value> = std::fs::read_to_string(paths.results())
        .unwrap_or_default()
        .lines()
        .filter_map(|row| serde_json::from_str::<Value>(row).ok())
        .filter(|row| row.get("ts_ms").and_then(Value::as_u64).unwrap_or(0) > since)
        .collect();
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
        let field = |name: &str| row.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        let dom = if row.get("dom").and_then(Value::as_bool).unwrap_or(false) { "DOM" } else { "INV" };
        out.push(format!("- [{dom}] {}: with it {}, without it {}: {} ({}; learned at {})", field("text"), field("with"), field("without"), field("verdict").replace('_', " "), field("evidence"), field("of")));
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

    std::fs::create_dir_all(&paths.reports).ok()?;
    std::fs::write(&file, out.join("\n")).ok()?;
    let _ = std::fs::write(paths.reports.join("last.json"), json!({"ts_ms": now_ms(), "file": file}).to_string());
    tracing::info!(report = %file.display(), "wrote the night report");
    Some(file)
}
