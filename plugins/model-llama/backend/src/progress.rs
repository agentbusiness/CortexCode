//! `model.progress` for one request: queued, prefill, decode and done, at most every 250 ms per
//! phase (a phase change is always published).

use cc_protocol::contracts::model::{PROGRESS_EVERY_MS, Priority, Progress, progress_phase, topics};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub type Publisher = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

static NEXT: AtomicU64 = AtomicU64::new(1);

pub struct Tracker {
    publish: Publisher,
    event: Progress,
    started: Instant,
    last: Option<Instant>,
    first_token: Option<Instant>,
}

impl Tracker {
    /// Publishes `queued`.
    pub fn new(publish: Publisher, cache_key: &str, priority: Priority, work: &str) -> Self {
        let event = Progress {
            request: format!("llama-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            cache_key: cache_key.to_owned(),
            priority,
            work: work.to_owned(),
            phase: progress_phase::QUEUED.into(),
            ..Progress::default()
        };
        let mut tracker = Self { publish, event, started: Instant::now(), last: None, first_token: None };
        tracker.emit(true);
        tracker
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.last.is_some_and(|last| now.duration_since(last).as_millis() < u128::from(PROGRESS_EVERY_MS)) {
            return;
        }
        self.last = Some(now);
        self.event.elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        if let Ok(value) = serde_json::to_value(&self.event) {
            (self.publish)(topics::PROGRESS, value);
        }
    }

    fn phase(&mut self, phase: &str) -> bool {
        if self.event.phase == phase {
            return false;
        }
        self.event.phase = phase.to_owned();
        true
    }

    pub fn prefill(&mut self, total: u32, cached: u32, processed: u32) {
        self.event.prompt_tokens = total;
        self.event.cached_tokens = cached;
        self.event.prefill_total = total.saturating_sub(cached);
        self.event.prefill_done = processed.saturating_sub(cached).min(self.event.prefill_total);
        let changed = self.phase(progress_phase::PREFILL);
        self.emit(changed);
    }

    pub fn token(&mut self) {
        let now = Instant::now();
        let first = *self.first_token.get_or_insert(now);
        self.event.generated += 1;
        self.event.prefill_done = self.event.prefill_total;
        let seconds = now.duration_since(first).as_secs_f64();
        self.event.tok_s = if seconds > 0.0 { f64::from(self.event.generated.saturating_sub(1)) / seconds } else { 0.0 };
        let changed = self.phase(progress_phase::DECODE);
        self.emit(changed);
    }

    pub fn done(&mut self, finish: &str, generated: Option<u32>) {
        if let Some(generated) = generated {
            self.event.generated = generated;
        }
        self.event.finish = finish.to_owned();
        self.phase(progress_phase::DONE);
        self.emit(true);
    }
}
