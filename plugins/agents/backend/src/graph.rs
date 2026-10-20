//! Rendering code-graph results as text for the planner.

use cc_protocol::contracts::memory::{CallSite, ImpactResult, OutlineResult, RepoMapResult, SearchResult, SymbolMatches, SymbolRef, methods as memory_methods};
use cc_protocol::contracts::memory::EdgesResult;
use cc_protocol::contracts::tools::{ReadResult, methods as tools_methods};
use serde_json::Value;

fn symbol_line(symbol: &SymbolRef) -> String {
    let mut line = format!("{} `{}` {}:{}-{}", symbol.kind, symbol.qname, symbol.path, symbol.start_line, symbol.end_line);
    if !symbol.signature.is_empty() {
        line.push_str(&format!("\n  signature: {}", symbol.signature));
    }
    if let Some(description) = symbol.description.as_deref().filter(|text| !text.is_empty()) {
        line.push_str(&format!("\n  {description}"));
    }
    line
}

fn site_line(site: &CallSite) -> String {
    format!(
        "{}:{} in `{}` calls `{}`({})",
        site.path,
        site.line,
        site.caller.qname,
        site.callee.name,
        site.args.join(", ")
    )
}

/// Render a read's JSON result as text for the model. Unknown shapes are shown as JSON.
pub fn render(tool: &str, value: &Value) -> String {
    let rendered = match tool {
        memory_methods::SEARCH => serde_json::from_value::<SearchResult>(value.clone()).ok().map(|result| {
            if result.hits.is_empty() {
                return "No matches.".to_owned();
            }
            result.hits.iter().map(|hit| symbol_line(&hit.symbol)).collect::<Vec<_>>().join("\n")
        }),
        memory_methods::SYMBOL => serde_json::from_value::<SymbolMatches>(value.clone()).ok().map(|result| {
            if result.matches.is_empty() {
                return "No symbol by that name.".to_owned();
            }
            result
                .matches
                .iter()
                .map(|detail| {
                    let mut text = symbol_line(&detail.symbol);
                    text.push_str(&format!("\n  callers: {}, callees: {}", detail.callers, detail.callees));
                    if !detail.tests.is_empty() {
                        let tests: Vec<String> = detail.tests.iter().map(|test| format!("{} ({})", test.qname, test.path)).collect();
                        text.push_str(&format!("\n  tests: {}", tests.join(", ")));
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }),
        memory_methods::CALLERS | memory_methods::CALLEES => {
            serde_json::from_value::<EdgesResult>(value.clone()).ok().map(|result| {
                if result.sites.is_empty() {
                    return "No call sites.".to_owned();
                }
                result.sites.iter().map(site_line).collect::<Vec<_>>().join("\n")
            })
        }
        memory_methods::IMPACT => serde_json::from_value::<ImpactResult>(value.clone()).ok().map(|result| render_impact(&result)),
        memory_methods::OUTLINE => serde_json::from_value::<OutlineResult>(value.clone()).ok().map(|result| {
            let mut text = format!("{} ({})", result.path, result.language);
            for symbol in &result.symbols {
                text.push_str(&format!("\n{}", symbol_line(symbol)));
            }
            text
        }),
        memory_methods::REPO_MAP => serde_json::from_value::<RepoMapResult>(value.clone()).ok().map(|result| result.text),
        tools_methods::READ => serde_json::from_value::<ReadResult>(value.clone()).ok().map(|result| result.text),
        _ => None,
    };
    rendered.unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

pub fn render_impact(result: &ImpactResult) -> String {
    let mut text = String::new();
    if !result.changed.is_empty() {
        text.push_str("Symbols:");
        for symbol in &result.changed {
            text.push_str(&format!("\n{}", symbol_line(symbol)));
        }
    }
    if !result.call_sites.is_empty() {
        text.push_str("\nCall sites:");
        for site in &result.call_sites {
            text.push_str(&format!("\n{}", site_line(site)));
        }
    }
    if !result.tests.is_empty() {
        text.push_str("\nTests:");
        for test in &result.tests {
            text.push_str(&format!("\n{} ({})", test.qname, test.path));
        }
    }
    if !result.rules.is_empty() {
        text.push_str(&format!("\nRules: {}", result.rules.join("; ")));
    }
    if text.is_empty() { "No impact found.".to_owned() } else { text.trim_start().to_owned() }
}

