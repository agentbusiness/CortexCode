//! Live metrics for the agents scheduler: throughput over a recent window of completed requests and
//! the measured cost of hibernation, reported by `model.slots`.

use cc_protocol::contracts::model::{HibernationCost, LiveMetrics, Usage};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window the recent rates cover.
pub const WINDOW: Duration = Duration::from_secs(60);

struct Sample {
    at: Instant,
    processed: u64,
    cached: u64,
    prefill_ms: f64,
    completion: u64,
    decode_ms: f64,
    queue_ms: f64,
}

#[derive(Default)]
struct Transfers {
    count: u32,
    tokens: u64,
    bytes: u64,
    ms: f64,
}

#[derive(Default)]
struct Inner {
    samples: VecDeque<Sample>,
    saves: Transfers,
    restores: Transfers,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn request(&self, usage: &Usage) {
        let mut inner = self.lock();
        let now = Instant::now();
        while inner.samples.front().is_some_and(|sample| now.duration_since(sample.at) > WINDOW) {
            inner.samples.pop_front();
        }
        inner.samples.push_back(Sample {
            at: now,
            processed: u64::from(usage.prompt_tokens.saturating_sub(usage.cached_tokens)),
            cached: u64::from(usage.cached_tokens),
            prefill_ms: usage.prefill_ms,
            completion: u64::from(usage.completion_tokens),
            decode_ms: usage.decode_ms,
            queue_ms: usage.queue_ms,
        });
    }

    pub fn saved(&self, tokens: u64, bytes: u64, ms: f64) {
        let mut inner = self.lock();
        inner.saves.count += 1;
        inner.saves.tokens += tokens;
        inner.saves.bytes += bytes;
        inner.saves.ms += ms;
    }

    pub fn restored(&self, tokens: u64, bytes: u64, ms: f64) {
        let mut inner = self.lock();
        inner.restores.count += 1;
        inner.restores.tokens += tokens;
        inner.restores.bytes += bytes;
        inner.restores.ms += ms;
    }

    pub fn snapshot(&self) -> LiveMetrics {
        let mut inner = self.lock();
        let now = Instant::now();
        while inner.samples.front().is_some_and(|sample| now.duration_since(sample.at) > WINDOW) {
            inner.samples.pop_front();
        }
        let samples = &inner.samples;
        let requests = samples.len() as u32;
        let sum = |f: &dyn Fn(&Sample) -> f64| samples.iter().map(f).sum::<f64>();
        let completion = sum(&|s| s.completion as f64);
        let decode_ms = sum(&|s| s.decode_ms);
        let processed = sum(&|s| s.processed as f64);
        let prefill_ms = sum(&|s| s.prefill_ms);
        let cached = sum(&|s| s.cached as f64);
        // The window actually covered: from the earliest completion's start of decode to now.
        let covered = samples
            .front()
            .map(|first| now.duration_since(first.at).as_secs_f64() + first.decode_ms / 1000.0)
            .unwrap_or(0.0)
            .min(WINDOW.as_secs_f64());
        let rate = |numerator: f64, ms: f64| if ms > 0.0 { numerator / ms * 1000.0 } else { 0.0 };
        let mean = |total: f64, count: u32| if count > 0 { total / f64::from(count) } else { 0.0 };
        let mb_s = |bytes: u64, ms: f64| if ms > 0.0 { bytes as f64 / 1e6 / (ms / 1000.0) } else { 0.0 };
        LiveMetrics {
            window_s: covered,
            requests,
            decode_tps_aggregate: if covered > 0.0 { completion / covered } else { 0.0 },
            decode_tps_per_stream: rate(completion, decode_ms),
            prefill_tps: rate(processed, prefill_ms),
            cache_hit_ratio: if processed + cached > 0.0 { cached / (processed + cached) } else { 0.0 },
            mean_queue_ms: mean(sum(&|s| s.queue_ms), requests),
            hibernation: HibernationCost {
                saves: inner.saves.count,
                restores: inner.restores.count,
                bytes_per_token: if inner.saves.tokens > 0 {
                    inner.saves.bytes as f64 / inner.saves.tokens as f64
                } else {
                    0.0
                },
                save_ms_mean: mean(inner.saves.ms, inner.saves.count),
                restore_ms_mean: mean(inner.restores.ms, inner.restores.count),
                save_mb_s: mb_s(inner.saves.bytes, inner.saves.ms),
                restore_mb_s: mb_s(inner.restores.bytes, inner.restores.ms),
            },
        }
    }
}

