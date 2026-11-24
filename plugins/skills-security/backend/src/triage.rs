//! The filter: what a finding means for the person, decided in this order.
//!
//! 1. A mute rule the person made covers it: `muted`.
//! 2. Host rules on structural facts decide what they can: a malicious package, a campaign
//!    indicator, planted persistence or a live credential is an alert; an exploited vulnerability
//!    the code reaches is an alert; a development-only dependency or a low-severity one nobody
//!    exploits is quiet; a hardening note is quiet; a finding the audit confirmed with evidence is
//!    an alert.
//! 3. The rest goes to the local model in one short constrained call: the system text, the attack
//!    class's decision card (`data/triage/card-<class>.md`) and the facts the host has. The model
//!    writes one sentence of reason, then `dismiss`, `hardening`, `alert` or `investigate`.

use cc_protocol::contracts::memory::SecurityFinding;
use cc_protocol::contracts::model::{ChatMessage, ChatParams, ChatResult, Priority, Sampling, Thinking};
use cc_protocol::contracts::security::MuteRule;
use cc_protocol::{Ctx, RpcError, parse_params};
use serde_json::{Value, json};
use std::path::Path;

/// A judgment: the triage value and its reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub triage: String,
    pub reason: String,
    pub by: String,
    pub muted_by: Option<String>,
}

impl Judgment {
    fn host(triage: &str, reason: impl Into<String>) -> Self {
        Self { triage: triage.into(), reason: reason.into(), by: "host".into(), muted_by: None }
    }
}

fn rank(severity: Option<&str>) -> i32 {
    match severity.unwrap_or("").to_ascii_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" | "moderate" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Whether a signal says the code already ran or a credential is out: never waits for a model.
fn hard_signal(finding: &SecurityFinding) -> Option<&str> {
    let signal = finding.signal.as_deref()?;
    ["malicious package", "campaign indicator", "live credential"].into_iter().find(|s| signal.contains(s))
}

fn is_test_path(path: &str) -> bool {
    let path = format!("/{}", path.to_ascii_lowercase());
    ["/test/", "/tests/", "/spec/", "/__tests__/", "/fixtures/", "/fixture/", "/testdata/", "/examples/", "/example/", "/mocks/"].iter().any(|part| path.contains(part))
        || path.contains(".test.")
        || path.contains(".spec.")
        || path.rsplit('/').next().is_some_and(|name| name.starts_with("test_"))
}

// ---- mute rules ---------------------------------------------------------------------------------

/// The finding's package and ecosystem: from its fleet key (`dep:<eco>:<name>:<version>`), else
/// its quick fix.
pub fn package_of(finding: &SecurityFinding) -> Option<(String, String)> {
    if let Some(key) = finding.fleet_key.as_deref().and_then(|k| k.strip_prefix("dep:")) {
        // `<eco>:<name>:<version>`, where a Maven name holds a colon: the version is after the last one.
        let (eco, rest) = key.split_once(':')?;
        let name = rest.rsplit_once(':').map_or(rest, |(name, _)| name);
        return Some((eco.to_owned(), name.to_owned()));
    }
    let fix = finding.quick_fix.as_ref()?;
    Some((fix.ecosystem.clone(), fix.package.to_ascii_lowercase()))
}

/// Whether a mute rule covers a finding of a project (a rule past its end covers nothing).
pub fn mute_covers(rule: &MuteRule, project_id: &str, finding: &SecurityFinding, now: u64) -> bool {
    if rule.until_ms.is_some_and(|until| until <= now) {
        return false;
    }
    match rule.kind.as_str() {
        "finding" => rule.project_id.as_deref() == Some(project_id) && rule.finding_id.as_deref() == Some(finding.id.as_str()),
        "similar" => rule.fleet_key.is_some() && rule.fleet_key == finding.fleet_key,
        "package" => match (package_of(finding), &rule.ecosystem, &rule.package) {
            (Some((eco, name)), Some(rule_eco), Some(rule_name)) => eco.eq_ignore_ascii_case(rule_eco) && name.eq_ignore_ascii_case(rule_name),
            _ => false,
        },
        "advisory" => rule.advisory.as_ref().is_some_and(|advisory| finding.advisories.iter().any(|a| a.eq_ignore_ascii_case(advisory))),
        "path" => {
            rule.project_id.as_deref() == Some(project_id)
                && rule.class.as_deref() == Some(finding.class.as_str())
                && rule.path_prefix.as_ref().is_some_and(|prefix| finding.sink_path.as_deref().is_some_and(|path| path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))))
        }
        _ => false,
    }
}

// ---- host rules ---------------------------------------------------------------------------------

/// What the host decides from structural facts alone; None leaves it to the model.
pub fn host_rule(finding: &SecurityFinding) -> Option<Judgment> {
    if finding.verdict == "rejected" {
        return Some(Judgment::host("quiet", "rejected by the audit with the fact that disproves it"));
    }
    if let Some(signal) = hard_signal(finding) {
        return Some(Judgment::host("alert", format!("{signal}: this is acted on whatever else holds")));
    }
    if finding.advisories.iter().any(|id| id.starts_with("MAL-")) {
        return Some(Judgment::host("alert", "a malicious package is pinned: it may already have run at install"));
    }
    let reachable = finding.reachable.as_deref();
    let severity = rank(finding.severity_overall.as_deref());
    let epss = finding.epss.unwrap_or(0.0);
    if finding.class == "dependencies" {
        if finding.kev && !matches!(reachable, Some("dev" | "not_imported")) {
            return Some(Judgment::host("alert", "exploited in the wild (CISA KEV) and the code can reach it"));
        }
        if reachable == Some("called") && severity >= 3 {
            return Some(Judgment::host("alert", "the code calls a function the advisory names as vulnerable"));
        }
        if reachable == Some("dev") && severity < 4 && !finding.kev {
            return Some(Judgment::host("quiet", "a development-only dependency: it never ships to users"));
        }
        if severity <= 1 && !finding.kev && epss < 0.01 {
            return Some(Judgment::host("quiet", "low severity, not exploited in the wild, exploitation unlikely (EPSS under 1%)"));
        }
        if matches!(reachable, Some("not_imported" | "transitive")) && severity <= 2 && !finding.kev && epss < 0.05 {
            return Some(Judgment::host("quiet", "not imported by the project's code, medium severity at most, not exploited"));
        }
    }
    if finding.verdict == "hardening" {
        return Some(Judgment::host("quiet", format!("hardening note: {}", if finding.fix_change.is_empty() { finding.title.clone() } else { finding.fix_change.clone() })));
    }
    if finding.verdict == "confirmed" && finding.source != "sweep" {
        return Some(Judgment::host("alert", "the audit confirmed it with evidence"));
    }
    if finding.signal.as_deref().is_some_and(|s| s.contains("real key format")) && finding.sink_path.as_deref().is_some_and(is_test_path) {
        return None;
    }
    if finding.signal.as_deref().is_some_and(|s| s.contains("real key format") || s.contains("secret in git history")) {
        return Some(Judgment::host("alert", "a real credential format outside tests: anyone with a copy of the repository has it"));
    }
    None
}

/// Whether an alert is told at once (the rest wait for the daily digest).
pub fn urgent(finding: &SecurityFinding) -> bool {
    hard_signal(finding).is_some()
        || finding.advisories.iter().any(|id| id.starts_with("MAL-"))
        || finding.kev
        || rank(finding.severity_overall.as_deref()) >= 4
        || (rank(finding.severity_overall.as_deref()) >= 3 && finding.reachable.as_deref() == Some("called"))
}

// ---- the model ----------------------------------------------------------------------------------

/// The decision cards and the system text, read from the data folder in use on every call (the
/// app edits them live).
pub struct Cards<'a> {
    pub dir: &'a Path,
}

impl Cards<'_> {
    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
    }

    pub fn system(&self) -> Result<String, RpcError> {
        self.read("system.md").ok_or_else(|| RpcError::internal(format!("the filter's system text {} is missing", self.dir.join("system.md").display())))
    }

    pub fn card(&self, class: &str) -> String {
        self.read(&format!("card-{class}.md")).or_else(|| self.read("card-default.md")).unwrap_or_default()
    }

    pub fn investigation(&self) -> Option<String> {
        self.read("investigate.md")
    }
}

/// The facts the host has about a finding, one per line, as the model reads them.
pub fn facts(finding: &SecurityFinding) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |label: &str, value: &str| {
        let value = value.trim();
        if !value.is_empty() {
            out.push(format!("{label}: {value}"));
        }
    };
    add("Finding", &finding.title);
    add("Class", &finding.class);
    add("Verdict of the check", &finding.verdict.replace('_', " "));
    add("Severity", finding.severity_overall.as_deref().unwrap_or("not rated"));
    add("Found by", &finding.source);
    let place = match (finding.sink_path.as_deref(), finding.sink_line) {
        (Some(path), Some(line)) if line > 0 => format!("{path}:{line}"),
        (Some(path), _) => path.to_owned(),
        _ => String::new(),
    };
    add("Where", &place);
    if let Some(path) = finding.sink_path.as_deref() {
        add("Test or example file", if is_test_path(path) { "yes" } else { "no" });
    }
    add("What the check saw", &finding.evidence_observed);
    add("Who could use it", &finding.principal);
    add("What it allows", &finding.result);
    add("What stops it", &finding.control);
    add("Signal", finding.signal.as_deref().unwrap_or(""));
    add("Reachable from the project's code", finding.reachable.as_deref().unwrap_or("not known"));
    if !finding.advisories.is_empty() {
        add("Advisories", &finding.advisories.join(", "));
    }
    if finding.kev {
        add("Exploited in the wild", "yes (CISA KEV)");
    }
    if let Some(epss) = finding.epss {
        add("Chance of exploitation in 30 days (EPSS)", &format!("{:.1}%", epss * 100.0));
    }
    add("The fact that decides it", finding.missing_fact.as_deref().unwrap_or(""));
    add("Fix", &finding.fix_change);
    out
}

/// The filter's schema: the reason first (the deciding fact), then the choice.
fn schema(choices: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "reason": {"type": "string", "minLength": 8, "maxLength": 240},
            "decision": {"type": "string", "enum": choices}
        },
        "required": ["reason", "decision"],
        "additionalProperties": false
    })
}

/// One constrained call: (decision, reason).
pub async fn ask(ctx: &Ctx, system: &str, card: &str, facts: &[String], choices: &[&str]) -> Result<(String, String), RpcError> {
    let user = format!("Decision card:\n{card}\n\nFacts the host checked:\n{}\n\nDecide for this finding.", facts.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"));
    let params = ChatParams {
        messages: vec![ChatMessage::new("system", system), ChatMessage::new("user", user)],
        tools: Vec::new(),
        tool_choice: None,
        grammar: None,
        json_schema: Some(schema(choices)),
        sampling: Sampling { temperature: 0.0, ..Sampling::default() },
        thinking: Thinking { enabled: false, effort: None, budget_tokens: None },
        priority: Priority::Background,
        cache_key: Some("security-filter".into()),
        stop: Vec::new(),
        max_tokens: None,
        idle_only: false,
    };
    let result: ChatResult = parse_params(ctx.call("model.chat", serde_json::to_value(&params)?).await?)?;
    let answer: Value = serde_json::from_str(result.content.trim()).map_err(|error| RpcError::internal(format!("the filter's answer is not JSON ({error})")))?;
    let decision = answer.get("decision").and_then(Value::as_str).unwrap_or_default().to_owned();
    let reason = answer.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    if !choices.contains(&decision.as_str()) {
        return Err(RpcError::internal(format!("the filter answered `{decision}`")));
    }
    Ok((decision, reason))
}

/// The model's judgment of a finding.
pub async fn model_judgment(ctx: &Ctx, cards: &Cards<'_>, finding: &SecurityFinding, investigate: bool) -> Result<Judgment, RpcError> {
    let system = cards.system()?;
    let card = cards.card(&finding.class);
    let (decision, reason) = ask(ctx, &system, &card, &facts(finding), &["dismiss", "hardening", "alert", "investigate"]).await?;
    let (triage, reason) = match decision.as_str() {
        "dismiss" => ("quiet", reason),
        "hardening" => ("quiet", format!("hardening: {reason}")),
        "investigate" if investigate => ("investigating", reason),
        // Without investigations what the model could not settle stays shown.
        "investigate" => ("alert", format!("not settled by the fast check: {reason}")),
        _ => ("alert", reason),
    };
    Ok(Judgment { triage: triage.into(), reason, by: "model".into(), muted_by: None })
}
