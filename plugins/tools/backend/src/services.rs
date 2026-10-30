//! Shared services as model tools (`cc_protocol::contracts::service`, docs/STANDARD-CHANGES.md):
//! the services this machine's apps offer (`mesh.services`, those a session may call from here)
//! become one tool each, named by the service (`schedule`, `trace`), with an `action` for the
//! method. A call goes through `mesh.service.call`; the host fills in who asked and never passes
//! `approved` from the model. A service CortexCode has its own tool for (`terminal.command`: the
//! `run` tool) is not offered again.

use crate::host::Host;
use cc_protocol::contracts::mesh::{OfferedService, ServicesResult};
use cc_protocol::contracts::model::{ToolCall, ToolSpec};
use cc_protocol::contracts::service::{self as svc, Share};
use cc_protocol::contracts::tools::{ExecContext, ToolOutcome};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Services CortexCode does not offer the model: it has its own tool for the work.
const OWN: &[&str] = &["terminal.command"];
/// The tools CortexCode has itself; a service of the same name is left out.
const BUILT_IN: &[&str] = &["read", "edit", "run", "terminal"];
/// How long the list of services is reused.
const FRESH: Duration = Duration::from_secs(30);
/// A tool result longer than this is cut (the service's read sizes keep it short when asked well).
const OUTPUT_CAP: usize = 6000;

static CACHE: Mutex<Option<(Instant, Vec<Share>)>> = Mutex::new(None);

/// The services a session here may use as tools: on this machine (this app or another app on it).
async fn offered(host: &dyn Host) -> Vec<Share> {
    if let Ok(cache) = CACHE.lock()
        && let Some((at, shares)) = cache.as_ref()
        && at.elapsed() < FRESH
    {
        return shares.clone();
    }
    let answer = tokio::time::timeout(Duration::from_secs(5), host.call("mesh.services", json!({}))).await;
    let services: Vec<OfferedService> = answer.ok().and_then(Result::ok).and_then(|v| serde_json::from_value::<ServicesResult>(v).ok()).map(|r| r.services).unwrap_or_default();
    let mut shares: Vec<Share> = Vec::new();
    for offer in services.into_iter().filter(|o| o.local && o.reachable) {
        let (name, _, _) = svc::tool_of(&offer.share);
        if OWN.contains(&offer.share.service.as_str()) || BUILT_IN.contains(&name.as_str()) || shares.iter().any(|s| s.service == offer.share.service) {
            continue;
        }
        shares.push(offer.share);
    }
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), shares.clone()));
    }
    shares
}

/// The services' tool specs, to add to CortexCode's own.
pub async fn specs(host: &dyn Host) -> Vec<ToolSpec> {
    offered(host)
        .await
        .iter()
        .map(|share| {
            let (name, description, parameters) = svc::tool_of(share);
            ToolSpec { name, description, parameters }
        })
        .collect()
}

/// A call of a service tool, done; None when `call` is not one.
pub async fn execute(host: &dyn Host, context: &ExecContext, call: &ToolCall, started: Instant) -> Option<ToolOutcome> {
    let shares = offered(host).await;
    let share = shares.iter().find(|share| svc::tool_of(share).0 == call.name)?;
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let fail = |kind: &str, output: String| ToolOutcome {
        ok: false,
        output,
        display: Value::Null,
        tool: call.name.clone(),
        args: args.clone(),
        intent: String::new(),
        changed_files: Vec::new(),
        artifact: None,
        needs_approval: false,
        duration_ms: started.elapsed().as_millis() as u64,
        error_kind: Some(kind.into()),
    };
    let Some(method) = svc::method_of(share, action) else {
        let actions: Vec<String> = share.methods.iter().map(|m| svc::action_of(&share.service, &m.method)).collect();
        return Some(fail("bad_argument", format!("`{}` takes action {}.", call.name, actions.join(", "))));
    };
    let mut params = args.clone();
    if let Some(map) = params.as_object_mut() {
        map.remove("action");
        map.remove("approved");
        map.insert("caller".into(), json!({"session_id": context.session_id, "by": "agent"}));
    }
    let answer = host.call("mesh.service.call", json!({"method": method, "params": params})).await;
    Some(match answer {
        Ok(value) => {
            let mut output = serde_json::to_string_pretty(&value).unwrap_or_default();
            if output.len() > OUTPUT_CAP {
                let cut = (0..=OUTPUT_CAP).rev().find(|&at| output.is_char_boundary(at)).unwrap_or(0);
                output.truncate(cut);
                output.push_str("\n[cut: ask for a smaller size or fewer records]");
            }
            let refused = value.get("needs_approval").is_some_and(|v| !v.is_null());
            if refused {
                output.push_str("\n[not run: it needs the person's approval; say what it would do and ask them]");
            }
            ToolOutcome {
                ok: !refused,
                output,
                display: json!({"service": share.service, "method": method}),
                tool: call.name.clone(),
                args,
                intent: String::new(),
                changed_files: Vec::new(),
                artifact: None,
                needs_approval: refused,
                duration_ms: started.elapsed().as_millis() as u64,
                error_kind: refused.then(|| "needs_approval".to_owned()),
            }
        }
        Err(error) => fail("service_error", format!("{method}: {}", error.message)),
    })
}
