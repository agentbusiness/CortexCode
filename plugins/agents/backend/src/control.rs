//! The concurrency controller: how many agents are in flight at once.
//!
//! A structural control loop over measured numbers (no judgement about meaning):
//! - **Foreground guard.** When agents slow the foreground down, the limit steps down and a
//!   ceiling stays at that level until the foreground has been within target for a while; then the
//!   limit returns to its starting level. "Slowed down" is measured, never inferred from a
//!   foreground call's own size: its wait for a model slot, its prefill time per uncached token
//!   against the same measure in windows with no agent running (a long cold prompt is slow alone
//!   too), and its time per output token.
//! - **Throughput search.** Otherwise, once the limit has been in force for a settle period and
//!   agents were actually waiting for a place (the limit was binding), the window's aggregate
//!   agent decode throughput is recorded for that limit. The limit then moves to the neighbour
//!   that is measured better: up while a higher limit raises throughput by at least `gain`, down
//!   when the lower limit does as well (fewer agents for the same throughput). Unmeasured
//!   neighbours are probed first. On a unimodal throughput curve this converges to its peak.
//! - **Re-probing.** After holding a level for `probe_after_ms`, neighbour measurements are
//!   forgotten so a changed machine (another model, other load) is found again.
//!
//! Agents waiting on tools hold no model slot (the chat flow hibernates their KV), so the limit
//! may exceed the model's slots; the bounds come from the data set.

use std::collections::BTreeMap;

/// Controller settings (data: `stages.toml` `[scheduler]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub min: u32,
    /// 0: no ceiling; the limit rises while measured throughput does.
    pub max: u32,
    pub settle_ms: u64,
    /// Longest a foreground call may wait for a model slot.
    pub foreground_queue_ms: f64,
    /// Foreground prefill time per uncached token may be at most this multiple of its value with
    /// no agent running.
    pub foreground_prefill_slowdown: f64,
    pub foreground_tpot_ms: f64,
    pub gain: f64,
    pub probe_after_ms: u64,
}

/// What one window of live metrics showed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Window {
    /// Agent tokens generated per second, all agents together.
    pub agent_tps: f64,
    /// Agent model calls finished in the window.
    pub agent_calls: u32,
    /// Foreground model calls finished in the window: their worst wait for a model slot, mean
    /// prefill time per uncached prompt token (0: no call prefilled enough tokens to measure) and
    /// mean time per output token.
    pub foreground_calls: u32,
    pub foreground_queue_ms: f64,
    pub foreground_prefill_ms_per_token: f64,
    pub foreground_tpot_ms: f64,
    /// Agents in flight and agents waiting for a place.
    pub running: u32,
    pub queued: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    pub from: u32,
    pub to: u32,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Controller {
    pub settings: Settings,
    pub limit: u32,
    /// Measured agent throughput per limit.
    pub samples: BTreeMap<u32, f64>,
    /// Upper bound while the foreground needs room.
    pub ceiling: Option<u32>,
    /// Foreground prefill time per uncached token measured in windows with no agent running.
    pub alone_prefill_ms_per_token: Option<f64>,
    /// The starting level, returned to when a foreground ceiling lifts.
    start: u32,
    changed_at: u64,
    last_violation: Option<u64>,
    held_since: Option<u64>,
    started: bool,
}

impl Settings {
    /// The highest limit: `max`, or none when it is 0.
    pub fn high(&self) -> u32 {
        if self.max == 0 { u32::MAX } else { self.max }
    }
}

impl Controller {
    pub fn new(settings: Settings, start: u32) -> Self {
        let limit = start.clamp(settings.min.max(1), settings.high().max(settings.min.max(1)));
        Self {
            settings,
            limit,
            samples: BTreeMap::new(),
            ceiling: None,
            alone_prefill_ms_per_token: None,
            start: limit,
            changed_at: 0,
            last_violation: None,
            held_since: None,
            started: false,
        }
    }

    fn upper(&self) -> u32 {
        self.ceiling.unwrap_or(u32::MAX).min(self.settings.high())
    }

    fn change(&mut self, now: u64, to: u32, reason: String) -> Option<Adjustment> {
        let to = to.clamp(self.settings.min.max(1), self.upper().max(self.settings.min.max(1)));
        if to == self.limit {
            return None;
        }
        let adjustment = Adjustment { from: self.limit, to, reason };
        self.limit = to;
        self.changed_at = now;
        self.held_since = None;
        Some(adjustment)
    }

    /// Replace the settings with a new data set's. A limit outside the new `[min, max]` moves into
    /// it at once (a narrowed range takes effect without waiting for the next throughput step), and
    /// throughput measured at levels outside the range is dropped.
    pub fn update_settings(&mut self, now: u64, settings: Settings) -> Option<Adjustment> {
        if settings == self.settings {
            return None;
        }
        self.settings = settings;
        let (low, high) = (self.settings.min.max(1), self.settings.high().max(1));
        self.samples.retain(|level, _| (low..=high).contains(level));
        let reason = format!("scheduler range changed to [{}, {}]", self.settings.min, self.settings.max);
        self.change(now, self.limit, reason)
    }

    /// One control step at time `now` (ms) over the latest window.
    pub fn tick(&mut self, now: u64, window: &Window) -> Option<Adjustment> {
        let s = self.settings.clone();
        // The starting limit is in force from the first step on.
        if !self.started {
            self.started = true;
            self.changed_at = now;
        }
        // The foreground's own prefill speed, from windows where no agent shared the model.
        if window.running == 0 && window.agent_calls == 0 && window.foreground_prefill_ms_per_token > 0.0 {
            let alone = window.foreground_prefill_ms_per_token;
            self.alone_prefill_ms_per_token = Some(match self.alone_prefill_ms_per_token {
                Some(previous) => 0.5 * previous + 0.5 * alone,
                None => alone,
            });
        }
        // Foreground guard.
        let waited = window.foreground_calls > 0 && window.foreground_queue_ms > s.foreground_queue_ms;
        let slow_prefill = window.foreground_prefill_ms_per_token > 0.0
            && self.alone_prefill_ms_per_token.is_some_and(|alone| window.foreground_prefill_ms_per_token > alone * s.foreground_prefill_slowdown);
        let slow_tokens = window.foreground_calls > 0 && window.foreground_tpot_ms > s.foreground_tpot_ms;
        if waited || slow_prefill || slow_tokens {
            let first_violation = self.last_violation.is_none_or(|at| now.saturating_sub(at) >= s.settle_ms);
            self.last_violation = Some(now);
            if self.limit > s.min.max(1) && (first_violation || now.saturating_sub(self.changed_at) >= s.settle_ms) {
                let to = self.limit - 1;
                self.ceiling = Some(to);
                let reason = if waited {
                    format!("foreground waited {:.0} ms for a model slot > target {:.0} ms", window.foreground_queue_ms, s.foreground_queue_ms)
                } else if slow_prefill {
                    format!(
                        "foreground prefill {:.2} ms/token > {:.1}x its {:.2} ms/token with no agent running",
                        window.foreground_prefill_ms_per_token,
                        s.foreground_prefill_slowdown,
                        self.alone_prefill_ms_per_token.unwrap_or_default()
                    )
                } else {
                    format!("foreground time per token {:.1} ms > target {:.1} ms", window.foreground_tpot_ms, s.foreground_tpot_ms)
                };
                // Throughput measured above the new ceiling no longer applies.
                self.samples.retain(|level, _| *level <= to);
                return self.change(now, to, reason);
            }
            return None;
        }
        // The ceiling lifts after the foreground has been within target for a probe period, and
        // the limit returns to its starting level (the throughput search goes on from there).
        if let (Some(_), Some(at)) = (self.ceiling, self.last_violation)
            && now.saturating_sub(at) >= s.probe_after_ms
        {
            self.ceiling = None;
            if self.limit < self.start {
                let reason = format!("foreground within target for {} s", s.probe_after_ms / 1000);
                return self.change(now, self.start, reason);
            }
        }
        // Throughput search, on settled and binding windows only.
        if now.saturating_sub(self.changed_at) < s.settle_ms || window.agent_calls == 0 {
            return None;
        }
        let binding = window.queued > 0 || window.running >= self.limit;
        if !binding {
            return None;
        }
        let level = self.limit;
        // Repeated measurements of one level are averaged (exponentially), so one noisy window
        // does not move the limit.
        let here = match self.samples.get(&level) {
            Some(previous) => 0.5 * previous + 0.5 * window.agent_tps,
            None => window.agent_tps,
        };
        self.samples.insert(level, here);
        let up = level + 1;
        let down = level.saturating_sub(1);
        let can_up = up <= self.upper();
        let can_down = level > s.min.max(1);
        let up_tps = self.samples.get(&up).copied();
        let down_tps = if can_down { self.samples.get(&down).copied() } else { None };
        // Probe unmeasured neighbours first: up while the level below was not better.
        if can_up && up_tps.is_none() && down_tps.is_none_or(|below| here >= below * (1.0 + s.gain)) {
            return self.change(now, up, format!("probing up: {here:.1} tokens/s at {level}"));
        }
        if can_down && down_tps.is_none() {
            return self.change(now, down, format!("probing down: {here:.1} tokens/s at {level}"));
        }
        if let Some(above) = up_tps.filter(|_| can_up)
            && above > here * (1.0 + s.gain)
        {
            return self.change(now, up, format!("{above:.1} tokens/s at {up} beats {here:.1} at {level}"));
        }
        if let Some(below) = down_tps
            && below * (1.0 + s.gain) >= here
        {
            return self.change(now, down, format!("{below:.1} tokens/s at {down} is as good as {here:.1} at {level}"));
        }
        // Holding the best level; re-probe after a while.
        let held = *self.held_since.get_or_insert(now);
        if now.saturating_sub(held) >= s.probe_after_ms {
            self.samples.retain(|other, _| *other == level);
            self.held_since = Some(now);
        }
        None
    }
}
