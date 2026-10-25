//! Every model call fits the conversation's share of the context. The prompt is counted exactly
//! (`model.count`: the model's own chat template and tokenizer) and, when it does not leave
//! `limits.answer_reserve_tokens` free for the answer, the conversation is compacted.
//!
//! Compaction is three marks on the session's `context` entry, which the projection honours:
//! `digested_through` (finished turns at or before it are shown as their user message, one note
//! listing their calls, and their final answer), `folded_through` (older tool outputs become one
//! line) and `steps_through` (layer 2: the running turn's steps at or before it become one note
//! listing their calls; its newest `limits.history_tail_steps` steps always stay whole). The
//! possible compactions are ordered least first ([`levels`]: older finished turns digested, then
//! older tool outputs folded, then the newest finished turn digested, then the running turn's older
//! steps) and the least
//! one that brings the prompt down to `limits.compact_target_tokens` is chosen by
//! bisection, checked by counting. Every compaction changes the conversation near its start, so the
//! model re-reads everything after that point (its recurrent state cannot be cut back): compacting
//! to the target, not merely to what fits, keeps those re-reads rare.
//!
//! [`prepare`] compacts earlier, at a wait: while the run's tools execute once the prompt is past
//! `limits.compact_early_tokens`, and when the turn ended past `limits.compact_idle_tokens`
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

/// The `context` entry's mark up to which in-place changes are shown ([`crate::projection`]):
/// the newest entry when the conversation was last compacted. Between compactions the
/// conversation only grows at its end, so every call continues the previous call's state.
pub const SETTLED: &str = "settled_through";

/// The compaction marks of the `context` entry. Both only grow: undoing one would change the
/// conversation near its start again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub digested_through: u64,
    pub folded_through: u64,
    /// Layer 2: the running turn's steps at or before it are one note listing their calls.
    pub steps_through: u64,
}

impl Marks {
    fn of(entry: &JournalEntry) -> Self {
        let mark = |field: &str| entry.data.get(field).and_then(Value::as_u64).unwrap_or(0);
        Self { digested_through: mark("digested_through"), folded_through: mark("folded_through"), steps_through: mark("steps_through") }
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
        Some((marks, most)) if most <= budget => {
            tracing::info!(session = %env.session_id, tokens, compacted = most, ?marks, "compacted to fit");
            Ok(build(&journal.entries_vec()))
        }
        Some((_, most)) => Err(full(env, most, budget)),
        None => Err(full(env, tokens, budget)),
    }
}

/// Settle the conversation up to its newest entry when that saves at least
/// `limits.settle_min_tokens`: the in-place rewrites (outdated reads, earlier echoes of an edited
/// file, outputs of a command run again, replaced plans, the pinned spec) then apply from here on.
/// A settle makes the engine re-read the history after the system block once, so one that saves
/// little is not made (seen: 12k re-read every 8 steps for about 1k saved). True when settled.
pub async fn settle(env: &RunEnv, journal: &mut Journal, build: impl Fn(&[JournalEntry]) -> Vec<ChatMessage>) -> Result<bool, RpcError> {
    let Some((context_seq, current)) = journal.context_entry().map(|entry| (entry.seq, Marks::of(entry))) else { return Ok(false) };
    let newest = journal.last_seq();
    let settled_before = journal.context_entry().and_then(|entry| entry.data.get(SETTLED).and_then(Value::as_u64)).unwrap_or(0);
    if newest <= settled_before {
        return Ok(false);
    }
    let entries = journal.entries_vec();
    let (Ok(before), Ok(after)) = (count(env, &build(&entries)).await, count(env, &build(&with_marks(&entries, context_seq, current, newest))).await) else {
        return Ok(false);
    };
    let saved = before.saturating_sub(after);
    if saved < env.data.limits.settle_min_tokens {
        tracing::debug!(session = %env.session_id, before, after, "not settled: it would save too little");
        return Ok(false);
    }
    journal.set(context_seq, SETTLED, json!(newest)).await?;
    tracing::info!(session = %env.session_id, before, after, "settled");
    Ok(true)
}

/// Fold the running turn's steps so far into their one note (layer 2 through the newest entry) and
/// settle the conversation there. For a procedure a tool drives piece by piece (an audit of many
/// units), each piece then starts from a short history: the pieces done are one note, the tool's
/// state is on disk, and the engine reads the new piece's steps only, instead of compacting and
/// re-reading the whole past every few pieces. True when the mark moved.
pub async fn fold_steps(journal: &mut Journal) -> Result<bool, RpcError> {
    let Some((context_seq, current)) = journal.context_entry().map(|entry| (entry.seq, Marks::of(entry))) else { return Ok(false) };
    let newest = journal.last_seq();
    if newest <= current.steps_through {
        return Ok(false);
    }
    journal.set(context_seq, "steps_through", json!(newest)).await?;
    let settled_before = journal.context_entry().and_then(|entry| entry.data.get(SETTLED).and_then(Value::as_u64)).unwrap_or(0);
    if newest > settled_before {
        journal.set(context_seq, SETTLED, json!(newest)).await?;
    }
    Ok(true)
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
/// is past `trigger` tokens (`limits.compact_early_tokens` while tools run,
/// `limits.compact_idle_tokens` when the turn ended), compact it to the target and prefill the compacted
/// prompt in the background. `build` makes the prompt without a stage message (system and
/// projection): what every later call starts with. Best effort: a failure leaves the conversation
/// as it was, for [`fitted`] to handle.
pub async fn prepare(
    env: &RunEnv,
    journal: &mut Journal,
    before: u64,
    trigger: u32,
    when: crate::prewarm::When,
    build: impl Fn(&[JournalEntry]) -> Vec<ChatMessage>,
) -> Option<Prewarm> {
    let limits = &env.data.limits;
    if env.prefix.ctx_window == 0 || trigger == 0 {
        return None;
    }
    // A background agent waiting on a long tool hands its slot back (hibernation); a prewarm would
    // hold it.
    if env.priority == Priority::Agent && limits.hibernate_after_ms > 0 {
        return None;
    }
    let early = u64::from(trigger);
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
    // Only a compaction that reaches the target is made: one that saves little would be made again
    // at every wait, each time re-reading the conversation for nothing (seen: 163k -> 161k, then a
    // 12-minute re-read).
    let target = target(env);
    match compact(env, journal, &build, before, target, target).await {
        Ok(Some((marks, compacted))) if compacted <= target => tracing::info!(session = %env.session_id, tokens, compacted, ?marks, "compacted early"),
        Ok(Some((_, most))) => {
            tracing::debug!(session = %env.session_id, tokens, most, target, "no early compaction: none reaches the target");
            return None;
        }
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "early compaction failed");
            return None;
        }
    }
    let messages = crate::prewarm::closed(build(&upto(&journal.entries_vec(), before)));
    Some(crate::prewarm::send(&env.host, &env.cache_key(), messages, &env.prefix.tools, "the compacted conversation", false, when, |_| {}))
}

/// Tokens a prompt may take: the conversation's context less the answer's reserve.
fn budget(env: &RunEnv) -> u32 {
    env.prefix.ctx_window.saturating_sub(env.data.limits.answer_reserve_tokens)
}

/// Tokens a compaction brings the prompt down to.
fn target(env: &RunEnv) -> u32 {
    env.data.limits.compact_target_tokens.min(budget(env))
}

/// A compaction chosen but not yet written: its marks, the prompt's tokens with them, and the
/// `context` entry it goes on with the mark settled with it.
#[derive(Clone, Copy, Debug)]
pub struct Chosen {
    pub marks: Marks,
    pub tokens: u32,
    context_seq: u64,
    current: Marks,
    settled: u64,
}

/// The least compaction of the entries before `before` that brings `build`'s prompt to `target`
/// tokens (or the most there is, when none does), without writing it; `None` when nothing is left
/// to compact.
async fn choose(env: &RunEnv, journal: &Journal, build: &impl Fn(&[JournalEntry]) -> Vec<ChatMessage>, before: u64, target: u32) -> Result<Option<Chosen>, RpcError> {
    let entries = upto(&journal.entries_vec(), before);
    let Some((context_seq, current)) = journal.context_entry().map(|entry| (entry.seq, Marks::of(entry))) else { return Ok(None) };
    let levels = levels(&entries, current, env.data.limits.history_tail_steps);
    if levels.is_empty() {
        return Ok(None);
    }
    // A compaction re-reads the conversation anyway: what changed in place since the last one
    // (outdated reads, replaced plans, the spec) is settled with it, up to its newest entry.
    let settled = entries.last().map(|entry| entry.seq).unwrap_or(0);
    let (index, tokens) = least(&levels, target, |marks| {
        let messages = build(&with_marks(&entries, context_seq, marks, settled));
        async move { count(env, &messages).await }
    })
    .await?;
    Ok(Some(Chosen { marks: levels[index], tokens, context_seq, current, settled }))
}

/// Write a chosen compaction's marks on the `context` entry.
async fn apply(journal: &mut Journal, chosen: &Chosen) -> Result<(), RpcError> {
    let (marks, current, context_seq) = (chosen.marks, chosen.current, chosen.context_seq);
    if marks.digested_through != current.digested_through {
        journal.set(context_seq, "digested_through", json!(marks.digested_through)).await?;
    }
    if marks.folded_through != current.folded_through {
        journal.set(context_seq, "folded_through", json!(marks.folded_through)).await?;
    }
    if marks.steps_through != current.steps_through {
        journal.set(context_seq, "steps_through", json!(marks.steps_through)).await?;
    }
    let settled_before = journal.context_entry().and_then(|entry| entry.data.get(SETTLED).and_then(Value::as_u64)).unwrap_or(0);
    if chosen.settled > settled_before {
        journal.set(context_seq, SETTLED, json!(chosen.settled)).await?;
    }
    Ok(())
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
    let Some(chosen) = choose(env, journal, build, before, target).await? else { return Ok(None) };
    if chosen.tokens <= accept {
        apply(journal, &chosen).await?;
    }
    Ok(Some((chosen.marks, chosen.tokens)))
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
/// digested, then (layer 2) the running turn's older steps as one note, oldest first, keeping its
/// newest `tail_steps` steps whole. A turn still running is never digested whole.
pub fn levels(entries: &[JournalEntry], current: Marks, tail_steps: u32) -> Vec<Marks> {
    let mut ends: Vec<u64> = finished_runs(entries).into_values().filter(|end| *end > current.digested_through).collect();
    ends.sort_unstable();
    let newest = ends.pop();
    let steps = current.steps_through;
    let mut levels: Vec<Marks> = ends.iter().map(|end| Marks { digested_through: *end, folded_through: current.folded_through, steps_through: steps }).collect();
    let digested = levels.last().map(|marks| marks.digested_through).unwrap_or(current.digested_through);
    levels.extend(
        fold_candidates(entries).into_iter().filter(|seq| *seq > current.folded_through).map(|seq| Marks { digested_through: digested, folded_through: seq, steps_through: steps }),
    );
    if let Some(newest) = newest {
        let folded = levels.last().map(|marks| marks.folded_through).unwrap_or(current.folded_through);
        levels.push(Marks { digested_through: newest, folded_through: folded, steps_through: steps });
    }
    let most = levels.last().copied().unwrap_or(current);
    levels.extend(step_candidates(entries, tail_steps).into_iter().filter(|seq| *seq > steps).map(|seq| Marks { steps_through: seq, ..most }));
    levels
}

/// The marks layer 2 may set: for each step of the running turns but the newest `tail_steps`, the
/// seq right before the next step starts (so a step and its results are covered together).
pub fn step_candidates(entries: &[JournalEntry], tail_steps: u32) -> Vec<u64> {
    let finished = finished_runs(entries);
    let mut starts: Vec<u64> = Vec::new();
    let mut last_key: Option<(String, u64)> = None;
    for entry in entries {
        let in_step = matches!(entry.kind.as_str(), kinds::REASONING | kinds::ASSISTANT_MESSAGE)
            || (entry.kind == kinds::TOOL_CALL && !entry.data.get("host").and_then(Value::as_bool).unwrap_or(false));
        if !in_step {
            continue;
        }
        let run_id = entry.data.get("run_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let Some(step) = entry.data.get("step").and_then(Value::as_u64) else { continue };
        if finished.contains_key(&run_id) {
            continue;
        }
        let key = (run_id, step);
        if last_key.as_ref() != Some(&key) {
            starts.push(entry.seq);
            last_key = Some(key);
        }
    }
    let tail = tail_steps.max(1) as usize;
    if starts.len() <= tail {
        return Vec::new();
    }
    // A step ends right before the next one starts; the newest `tail` steps stay whole.
    starts[1..=starts.len() - tail].iter().map(|next| next - 1).collect()
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

/// The entries as they would be with the context entry's marks set to `marks` and settled through
/// `settled`.
fn with_marks(entries: &[JournalEntry], context_seq: u64, marks: Marks, settled: u64) -> Vec<JournalEntry> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            if entry.seq == context_seq
                && let Some(data) = entry.data.as_object_mut()
            {
                data.insert("digested_through".into(), json!(marks.digested_through));
                data.insert("folded_through".into(), json!(marks.folded_through));
                data.insert("steps_through".into(), json!(marks.steps_through));
                let before = data.get(SETTLED).and_then(Value::as_u64).unwrap_or(0);
                data.insert(SETTLED.into(), json!(settled.max(before)));
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
