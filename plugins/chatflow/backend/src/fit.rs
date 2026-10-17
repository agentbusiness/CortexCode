//! Every model call fits the conversation's share of the context. The prompt is counted exactly
//! (`model.count`: the model's own chat template and tokenizer) and, when it does not leave
//! `limits.answer_reserve_tokens` free for the answer, the conversation is compacted.
//!
//! Compaction is two marks on the session's `context` entry, which the projection honours:
//! `digested_through` (finished turns at or before it are shown as their user message, one note
//! listing their calls, and their final answer) and `folded_through` (older tool outputs become one
//! line). The possible compactions are ordered least first ([`levels`]: older finished turns
//! digested, then older tool outputs folded, then the newest finished turn digested) and the least
//! one that brings the prompt down to `limits.compact_target_percent` of the context is chosen by
//! bisection, checked by counting. Every compaction changes the conversation near its start, so the
//! model re-reads everything after that point (its recurrent state cannot be cut back): compacting
//! to the target, not merely to what fits, keeps those re-reads rare.
//!
//! [`prepare`] compacts earlier, at a wait: while the run's tools execute once the prompt is past
//! `limits.compact_early_percent`, and when the turn ended past `limits.compact_idle_percent`
//! (lower: the user reads the answer, so the next turn starts with room). It prefills the compacted prompt in the
//! background (`model.prewarm`), so the next call finds it warm instead of re-reading while the user
//! waits. The next model call cancels a prewarm still running; what it prefilled is kept.
//!
//! When even the most compaction does not fit, the call fails with [`CONTEXT_FULL`] and a plain
//! explanation instead of reaching the model with a prompt it must reject. The spec pinned after
//! the system prompt, plans, user messages and the newest tool output are never compacted.

use crate::agent_loop::RunEnv;
use crate::journal::Journal;
use crate::projection::finished_runs;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::model::{ChatMessage, CountResult, Priority, methods as model_methods};
use serde_json::{Value, json};
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Error code of a request whose prompt cannot fit the context even with everything compacted.
pub const CONTEXT_FULL: &str = "context_full";

/// The compaction marks of the `context` entry. Both only grow: undoing one would change the
/// conversation near its start again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub digested_through: u64,
    pub folded_through: u64,
}

impl Marks {
    fn of(entry: &JournalEntry) -> Self {
        let mark = |field: &str| entry.data.get(field).and_then(Value::as_u64).unwrap_or(0);
        Self { digested_through: mark("digested_through"), folded_through: mark("folded_through") }
    }
}

/// The messages `build` makes from the journal, compacted as far as needed to fit. `build` must
/// make the whole prompt of the call (system, projection, stage message).
pub async fn fitted(env: &RunEnv, journal: &mut Journal, build: impl Fn(&[JournalEntry]) -> Vec<ChatMessage>) -> Result<Vec<ChatMessage>, RpcError> {
    let messages = build(&journal.entries_vec());
    if env.prefix.ctx_window == 0 {
        // The model's size is not known (model.info failed): nothing to measure against.
        return Ok(messages);
    }
    let budget = budget(env);
    let tokens = count(env, &messages).await?;
    if tokens <= budget {
        return Ok(messages);
    }
    match compact(env, journal, &build, u64::MAX, target(env), budget).await? {
        Some((_, most)) if most <= budget => Ok(build(&journal.entries_vec())),
        Some((_, most)) => Err(full(env, most, budget)),
        None => Err(full(env, tokens, budget)),
    }
}

/// Compact to the target now (the model ran out of room while answering). False when nothing is
/// left to compact.
pub async fn compact_now(env: &RunEnv, journal: &mut Journal, build: impl Fn(&[JournalEntry]) -> Vec<ChatMessage>) -> Result<bool, RpcError> {
    if env.prefix.ctx_window == 0 {
        return Ok(false);
    }
    let budget = budget(env);
    Ok(compact(env, journal, &build, u64::MAX, target(env), budget).await?.is_some_and(|(_, tokens)| tokens <= budget))
}

/// A background prefill of a prompt prefix; dropping it cancels what is still running.
pub struct Prewarm(pub(crate) CancellationToken);

impl Drop for Prewarm {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// At a wait: when the conversation before `before` (entries with a lower seq; `u64::MAX` = all)
/// is past `share` percent of the context (`limits.compact_early_percent` while tools run,
/// `limits.compact_idle_percent` when the turn ended), compact it to the target and prefill the compacted
/// prompt in the background. `build` makes the prompt without a stage message (system and
/// projection): what every later call starts with. Best effort: a failure leaves the conversation
/// as it was, for [`fitted`] to handle.
pub async fn prepare(env: &RunEnv, journal: &mut Journal, before: u64, share: u32, build: impl Fn(&[JournalEntry]) -> Vec<ChatMessage>) -> Option<Prewarm> {
    let limits = &env.data.limits;
    if env.prefix.ctx_window == 0 || share == 0 {
        return None;
    }
    // A background agent waiting on a long tool hands its slot back (hibernation); a prewarm would
    // hold it.
    if env.priority == Priority::Agent && limits.hibernate_after_ms > 0 {
        return None;
    }
    let early = u64::from(env.prefix.ctx_window) * u64::from(share) / 100;
    // The newest call's prompt and answer bound the conversation since (its prompt held all of it
    // and a stage message): below the share no exact count is needed.
    let newest = journal.entries().filter(|entry| entry.kind == kinds::USAGE).last().map(|usage| {
        let tokens = |field: &str| usage.data.get(field).and_then(Value::as_u64).unwrap_or(0);
        (tokens("prompt_tokens"), tokens("completion_tokens"))
    });
    if let Some((prompt, completion)) = newest
        && prompt > 0
        && prompt + completion < early
    {
        return None;
    }
    let tokens = match count(env, &build(&upto(&journal.entries_vec(), before))).await {
        Ok(tokens) => tokens,
        Err(error) => {
            tracing::debug!(%error, "counting the prompt for an early compaction failed");
            return None;
        }
    };
    if u64::from(tokens) < early {
        return None;
    }
    // Only a compaction that ends below the share is made now: one that cannot would be made again
    // at every wait, each time re-reading the conversation.
    match compact(env, journal, &build, before, target(env), early.saturating_sub(1) as u32).await {
        Ok(Some((marks, compacted))) if u64::from(compacted) < early => tracing::info!(session = %env.session_id, tokens, compacted, ?marks, "compacted early"),
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(%error, "early compaction failed");
            return None;
        }
    }
    let messages = crate::prewarm::closed(build(&upto(&journal.entries_vec(), before)));
    Some(crate::prewarm::send(&env.host, &env.cache_key(), messages, &env.prefix.tools, "the compacted conversation", false, |_| {}))
}

/// Tokens a prompt may take: the conversation's context less the answer's reserve.
fn budget(env: &RunEnv) -> u32 {
    env.prefix.ctx_window.saturating_sub(env.data.limits.answer_reserve_tokens)
}

/// Tokens a compaction brings the prompt down to.
fn target(env: &RunEnv) -> u32 {
    let share = u64::from(env.prefix.ctx_window) * u64::from(env.data.limits.compact_target_percent) / 100;
    (share as u32).min(budget(env))
}

/// The least compaction of the entries before `before` that brings `build`'s prompt to `target`
/// tokens (or the most there is, when none does), applied when its prompt takes at most `accept`
/// tokens. Returns its marks and the prompt's tokens with them; `None` when nothing is left to
/// compact.
async fn compact(
    env: &RunEnv,
    journal: &mut Journal,
    build: &impl Fn(&[JournalEntry]) -> Vec<ChatMessage>,
    before: u64,
    target: u32,
    accept: u32,
) -> Result<Option<(Marks, u32)>, RpcError> {
    let entries = upto(&journal.entries_vec(), before);
    let Some((context_seq, current)) = journal.context_entry().map(|entry| (entry.seq, Marks::of(entry))) else { return Ok(None) };
    let levels = levels(&entries, current);
    if levels.is_empty() {
        return Ok(None);
    }
    let (index, tokens) = least(&levels, target, |marks| {
        let messages = build(&with_marks(&entries, context_seq, marks));
        async move { count(env, &messages).await }
    })
    .await?;
    let marks = levels[index];
    if tokens > accept {
        return Ok(Some((marks, tokens)));
    }
    if marks.digested_through != current.digested_through {
        journal.set(context_seq, "digested_through", json!(marks.digested_through)).await?;
    }
    if marks.folded_through != current.folded_through {
        journal.set(context_seq, "folded_through", json!(marks.folded_through)).await?;
    }
    Ok(Some((marks, tokens)))
}

/// The least of `levels` (ordered least compaction first; more never lengthens the prompt) whose
/// prompt takes at most `target` tokens, else the last one; with its tokens. Bisection: every
/// answer was counted.
async fn least<F, Fut>(levels: &[Marks], target: u32, count: F) -> Result<(usize, u32), RpcError>
where
    F: Fn(Marks) -> Fut,
    Fut: Future<Output = Result<u32, RpcError>>,
{
    let last = levels.len() - 1;
    let most = count(levels[last]).await?;
    if most > target {
        return Ok((last, most));
    }
    let (mut lo, mut hi, mut at_hi) = (0usize, last, most);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let tokens = count(levels[mid]).await?;
        if tokens <= target {
            (hi, at_hi) = (mid, tokens);
        } else {
            lo = mid + 1;
        }
    }
    Ok((hi, at_hi))
}

/// Every compaction beyond `current`, least first: finished turns digested oldest first (all but
/// the newest), then older tool outputs folded oldest first, then the newest finished turn
/// digested. A turn still running is never digested.
pub fn levels(entries: &[JournalEntry], current: Marks) -> Vec<Marks> {
    let mut ends: Vec<u64> = finished_runs(entries).into_values().filter(|end| *end > current.digested_through).collect();
    ends.sort_unstable();
    let newest = ends.pop();
    let mut levels: Vec<Marks> = ends.iter().map(|end| Marks { digested_through: *end, folded_through: current.folded_through }).collect();
    let digested = levels.last().map(|marks| marks.digested_through).unwrap_or(current.digested_through);
    levels.extend(fold_candidates(entries).into_iter().filter(|seq| *seq > current.folded_through).map(|seq| Marks { digested_through: digested, folded_through: seq }));
    if let Some(newest) = newest {
        let folded = levels.last().map(|marks| marks.folded_through).unwrap_or(current.folded_through);
        levels.push(Marks { digested_through: newest, folded_through: folded });
    }
    levels
}

/// The entries a fold may end at: tool outputs only, never the newest one and never a skill read
/// (the projection keeps those whole). Everything else (the spec pinned after the system prompt,
/// plans, decisions, user and host messages) is counted in full and never folded.
pub fn fold_candidates(entries: &[JournalEntry]) -> Vec<u64> {
    let skill_calls: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|entry| crate::skillsel::read_skill_uri(entry).is_some())
        .filter_map(|entry| entry.data.get("call_id").and_then(serde_json::Value::as_str))
        .collect();
    let results: Vec<u64> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_RESULT)
        .filter(|entry| !entry.data.get("call_id").and_then(serde_json::Value::as_str).is_some_and(|id| skill_calls.contains(id)))
        .map(|entry| entry.seq)
        .collect();
    let newest = results.last().copied().unwrap_or(0);
    results.into_iter().filter(|seq| *seq < newest).collect()
}

/// The entries with a seq below `before`.
fn upto(entries: &[JournalEntry], before: u64) -> Vec<JournalEntry> {
    entries.iter().filter(|entry| entry.seq < before).cloned().collect()
}

/// The entries as they would be with the context entry's marks set to `marks`.
fn with_marks(entries: &[JournalEntry], context_seq: u64, marks: Marks) -> Vec<JournalEntry> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            if entry.seq == context_seq
                && let Some(data) = entry.data.as_object_mut()
            {
                data.insert("digested_through".into(), json!(marks.digested_through));
                data.insert("folded_through".into(), json!(marks.folded_through));
            }
            entry
        })
        .collect()
}

async fn count(env: &RunEnv, messages: &[ChatMessage]) -> Result<u32, RpcError> {
    let counted: CountResult = env.host.get(model_methods::COUNT, json!({"messages": messages, "tools": env.prefix.tools})).await?;
    Ok(counted.tokens)
}

fn full(env: &RunEnv, tokens: u32, budget: u32) -> RpcError {
    RpcError::new(CONTEXT_FULL, env.data.text("context_full", &[("tokens", &tokens.to_string()), ("budget", &budget.to_string())]))
}
