//! The deep research report, `docs/research/<topic-slug>.md`: written by code from the synthesis and
//! the notes (the model never writes the report text around the citations, so every citation points
//! at a real note with a verbatim quote).

use crate::deep::JobState;
use cc_protocol::contracts::research::Note;
use std::collections::HashMap;
use std::fmt::Write as _;

/// Render the report. `today` is `YYYY-MM-DD`.
pub fn render(state: &JobState, notes: &[Note], today: &str) -> String {
    let job = &state.job;
    let by_id: HashMap<&str, &Note> = notes.iter().map(|n| (n.id.as_str(), n)).collect();
    // Sources are numbered in the order findings first cite them, then the rest.
    let mut order: Vec<String> = Vec::new();
    let synthesis = state.synthesis.clone().unwrap_or_default();
    for finding in &synthesis.findings {
        for id in &finding.notes {
            if let Some(note) = by_id.get(id.as_str())
                && !order.contains(&note.url)
            {
                order.push(note.url.clone());
            }
        }
    }
    for note in notes {
        if !order.contains(&note.url) {
            order.push(note.url.clone());
        }
    }
    let number = |url: &str| order.iter().position(|u| u == url).map(|i| i + 1).unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(out, "# Research: {}\n", job.topic);
    let _ = writeln!(
        out,
        "_CortexCode research, {today} (job `{}`): {} round(s), {} source(s) read, {} note(s). Written with the local model from notes only; every finding cites sources whose verbatim quotes are listed below._\n",
        job.job_id,
        job.round.max(1),
        job.sources_read,
        notes.len()
    );
    if !job.questions.is_empty() {
        out.push_str("## Questions\n\n");
        for (i, question) in job.questions.iter().enumerate() {
            let _ = writeln!(out, "{}. {question}", i + 1);
        }
        out.push('\n');
    }
    out.push_str("## Summary\n\n");
    let _ = writeln!(out, "{}\n", if synthesis.summary.is_empty() { "(no summary)" } else { synthesis.summary.as_str() });
    out.push_str("## Findings\n\n");
    if synthesis.findings.is_empty() {
        out.push_str("No finding could be grounded in the sources read.\n\n");
    }
    for finding in &synthesis.findings {
        let mut cited: Vec<usize> = finding.notes.iter().filter_map(|id| by_id.get(id.as_str())).map(|n| number(&n.url)).filter(|n| *n > 0).collect();
        cited.sort_unstable();
        cited.dedup();
        let refs: String = cited.iter().map(|n| format!("[{n}]")).collect();
        let _ = writeln!(out, "- {} {refs} (confidence: {}, {} source(s))", finding.statement, finding.confidence, cited.len());
    }
    out.push('\n');
    if !synthesis.open_questions.is_empty() {
        out.push_str("## Open questions\n\n");
        for question in &synthesis.open_questions {
            let _ = writeln!(out, "- {question}");
        }
        out.push('\n');
    }
    out.push_str("## Sources\n\n");
    for (i, url) in order.iter().enumerate() {
        let source_notes: Vec<&Note> = notes.iter().filter(|n| &n.url == url).collect();
        let Some(first) = source_notes.first() else { continue };
        let title = if first.title.is_empty() { url.as_str() } else { first.title.as_str() };
        let date = first.date.as_deref().unwrap_or("undated");
        let via = first.source.as_deref().map(|s| format!(", via {s}")).unwrap_or_default();
        let accessed = chrono::DateTime::from_timestamp_millis(first.created_ms as i64).map(|t| t.date_naive().to_string()).unwrap_or_default();
        let _ = writeln!(out, "{}. {title} ({date}{via}). <{url}> (read {accessed})", i + 1);
        for note in source_notes {
            let quote = note.quote.replace('\n', " ");
            let _ = writeln!(out, "   - {} — \"{}\"", note.claim, quote.trim());
        }
    }
    if order.is_empty() {
        out.push_str("No source yielded a note.\n");
    }
    out.push('\n');
    if !state.unread.is_empty() {
        out.push_str("## Sources that could not be read\n\n");
        for unread in &state.unread {
            let _ = writeln!(out, "- {} <{}>: {}", unread.title, unread.url, unread.why);
        }
        out.push('\n');
    }
    out.push_str("## Method\n\n");
    let _ = writeln!(out, "- Sources searched: {}", if state.sources_searched.is_empty() { "none answered".to_owned() } else { state.sources_searched.join(", ") });
    let _ = writeln!(out, "- Queries: {}", state.queries_run.iter().map(|q| format!("`{q}`")).collect::<Vec<_>>().join(", "));
    if let Some(since) = &state.params.since {
        let _ = writeln!(out, "- Only sources from {since} on.");
    }
    let _ = writeln!(
        out,
        "- Each source was read in parts by the model, which wrote notes with verbatim quotes; {} quote(s) that did not appear in their source were discarded. Confidence is high only when two or more sources support a finding.",
        state.dropped_quotes
    );
    out
}
