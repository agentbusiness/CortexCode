//! Per-request progress (`model.progress`) and the preemption of background prefills.
//!
//! The engine thread reports every request as it is queued, starts, reads its prompt, writes and
//! ends; within one phase at most every [`PROGRESS_EVERY_MS`] (a phase change is always reported),
//! so a 40 tok/s stream costs four events a second. A prefill-only request (`model.prewarm`, at
//! background priority) never delays a real one: a request of a higher class for the same
//! conversation, or one that finds no free slot, stops the running prewarm at its next step (what
//! it prefilled stays in the conversation's state and checkpoints, so the real request continues
//! from there), and a prewarm still waiting for a conversation that has a real request is dropped.

use cc_protocol::contracts::model::{PROGRESS_EVERY_MS, Priority, Progress};
use std::time::{Duration, Instant};

/// Throttles one request's events: due on a phase change, else when the interval has passed.
#[derive(Debug, Default, Clone)]
pub struct Reporter {
    last: Option<(&'static str, Instant)>,
}

impl Reporter {
    pub fn due(&mut self, phase: &'static str, now: Instant) -> bool {
        let due = match self.last {
            Some((last, at)) => last != phase || now.duration_since(at) >= Duration::from_millis(PROGRESS_EVERY_MS),
            None => true,
        };
        if due {
            self.last = Some((phase, now));
        }
        due
    }
}

/// Generated tokens per second since the first one (0 until two tokens exist).
pub fn tok_s(generated: u32, first: Option<Instant>, now: Instant) -> f64 {
    let Some(first) = first else { return 0.0 };
    let seconds = now.duration_since(first).as_secs_f64();
    if generated < 2 || seconds <= 0.0 {
        return 0.0;
    }
    f64::from(generated - 1) / seconds
}

/// What the scheduler knows of a running request when choosing a prewarm to stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Running<'a> {
    pub key: &'a str,
    pub priority: Priority,
    pub prewarm: bool,
}

fn class(priority: Priority) -> u8 {
    match priority {
        Priority::Foreground => 0,
        Priority::Agent => 1,
        Priority::Background => 2,
    }
}

/// The running prewarm a waiting request of `priority` for `key` stops, by index into `running`:
/// the one on its own conversation when that is a prewarm of a lower class; else, when no slot is
/// free (`slot_free` false), a prewarm of a lower class on another conversation (the lowest class,
/// the last started). None when nothing should stop.
pub fn prewarm_to_stop(running: &[Running<'_>], key: &str, priority: Priority, slot_free: bool) -> Option<usize> {
    let below = |other: &Running<'_>| other.prewarm && class(other.priority) > class(priority);
    if let Some(index) = running.iter().position(|other| other.key == key) {
        return below(&running[index]).then_some(index);
    }
    if slot_free {
        return None;
    }
    running
        .iter()
        .enumerate()
        .filter(|(_, other)| below(other))
        .max_by_key(|(index, other)| (class(other.priority), *index))
        .map(|(index, _)| index)
}

/// Whether a waiting prewarm for `key` is moot: a real request for the same conversation is
/// waiting or running (it reads the same prefix itself, and the prewarm would only roll its state
/// back afterwards).
pub fn prewarm_moot<'a>(key: &str, others: impl IntoIterator<Item = (&'a str, bool)>) -> bool {
    others.into_iter().any(|(other, prewarm)| other == key && !prewarm)
}

/// A progress event of a request (the fields every phase carries).
pub fn event(request: u64, key: &str, priority: Priority, prewarm: bool, phase: &str, prompt_tokens: usize) -> Progress {
    Progress {
        request: format!("r{request}"),
        cache_key: key.to_owned(),
        priority,
        work: if prewarm { "prewarm" } else { "generate" }.to_owned(),
        phase: phase.to_owned(),
        prompt_tokens: prompt_tokens as u32,
        ..Progress::default()
    }
}
