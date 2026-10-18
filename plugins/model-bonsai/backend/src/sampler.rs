//! Sampling on the host, from the logits llama.cpp hands back: a grammar's mask, the repetition and
//! presence penalties, then greedy or temperature / top-k / top-p / min-p with a per-request seed
//! (each draw comes from the seed and the draw's index, so a seed reproduces a sample).

use std::collections::{HashMap, VecDeque};

/// Penalty window of a presence penalty (generated tokens only, as in vLLM and the OpenAI API).
pub const PRESENCE_WINDOW: usize = 4096;
/// Penalty window of a repetition penalty (the prompt's last tokens included).
pub const REPETITION_WINDOW: usize = 64;

/// The contract's sampling, as this plugin applies it.
#[derive(Debug, Clone, PartialEq)]
pub struct SamplingPlan {
    /// 0: greedy.
    pub temperature: f32,
    pub top_k: u32,
    pub top_p: f32,
    pub min_p: f32,
    pub repeat_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub seed: u64,
}

pub struct Sampler {
    plan: SamplingPlan,
    draws: u64,
    /// The last tokens of the conversation, for the repetition penalty.
    recent: VecDeque<u32>,
    /// Generated tokens in the presence window, with their counts.
    generated: VecDeque<u32>,
    present: HashMap<u32, u32>,
    scratch: Vec<(u32, f32)>,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl Sampler {
    /// `context`: the prompt's tokens (its tail seeds the repetition window).
    pub fn new(plan: SamplingPlan, context: &[u32]) -> Self {
        let recent = context.iter().rev().take(REPETITION_WINDOW).rev().copied().collect();
        Self { plan, draws: 0, recent, generated: VecDeque::new(), present: HashMap::new(), scratch: Vec::new() }
    }

    /// Note a token that became part of the conversation (sampled, or fed like a forced end of
    /// reasoning).
    pub fn push(&mut self, token: u32) {
        if self.plan.repeat_penalty.is_some() {
            self.recent.push_back(token);
            while self.recent.len() > REPETITION_WINDOW {
                self.recent.pop_front();
            }
        }
        if self.plan.presence_penalty.is_some() {
            self.generated.push_back(token);
            *self.present.entry(token).or_insert(0) += 1;
            while self.generated.len() > PRESENCE_WINDOW {
                if let Some(old) = self.generated.pop_front()
                    && let Some(count) = self.present.get_mut(&old)
                {
                    *count -= 1;
                    if *count == 0 {
                        self.present.remove(&old);
                    }
                }
            }
        }
    }

    /// Sample from `logits` (masked already: disallowed tokens are -inf). None when nothing is
    /// allowed.
    pub fn sample(&mut self, logits: &mut [f32]) -> Option<u32> {
        if let Some(penalty) = self.plan.repeat_penalty {
            for &token in &self.recent {
                if let Some(logit) = logits.get_mut(token as usize)
                    && logit.is_finite()
                {
                    *logit = if *logit > 0.0 { *logit / penalty } else { *logit * penalty };
                }
            }
        }
        if let Some(penalty) = self.plan.presence_penalty {
            for &token in self.present.keys() {
                if let Some(logit) = logits.get_mut(token as usize) {
                    *logit -= penalty;
                }
            }
        }
        if self.plan.temperature <= 0.0 {
            return logits
                .iter()
                .enumerate()
                .filter(|(_, logit)| logit.is_finite())
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(index, _)| index as u32);
        }
        let temperature = self.plan.temperature;
        self.scratch.clear();
        self.scratch.extend(logits.iter().enumerate().filter(|(_, logit)| logit.is_finite()).map(|(index, logit)| (index as u32, *logit / temperature)));
        if self.scratch.is_empty() {
            return None;
        }
        let k = self.plan.top_k as usize;
        if k > 0 && k < self.scratch.len() {
            self.scratch.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
            self.scratch.truncate(k);
        }
        self.scratch.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        let max = self.scratch[0].1;
        let mut total = 0.0f64;
        for entry in &mut self.scratch {
            entry.1 = (entry.1 - max).exp();
            total += f64::from(entry.1);
        }
        // min-p against the most likely token, then top-p over what is left (both on the
        // probabilities at the sampling temperature).
        if self.plan.min_p > 0.0 {
            let floor = self.plan.min_p * self.scratch[0].1;
            self.scratch.retain(|entry| entry.1 >= floor);
            total = self.scratch.iter().map(|entry| f64::from(entry.1)).sum();
        }
        if self.plan.top_p > 0.0 && self.plan.top_p < 1.0 {
            let mut kept = 0.0f64;
            let mut cut = self.scratch.len();
            for (index, entry) in self.scratch.iter().enumerate() {
                kept += f64::from(entry.1) / total;
                if kept >= f64::from(self.plan.top_p) {
                    cut = index + 1;
                    break;
                }
            }
            self.scratch.truncate(cut.max(1));
            total = self.scratch.iter().map(|entry| f64::from(entry.1)).sum();
        }
        self.draws += 1;
        let draw = (splitmix(self.plan.seed ^ splitmix(self.draws)) >> 11) as f64 / (1u64 << 53) as f64 * total;
        let mut acc = 0.0f64;
        for entry in &self.scratch {
            acc += f64::from(entry.1);
            if acc >= draw {
                return Some(entry.0);
            }
        }
        self.scratch.last().map(|entry| entry.0)
    }
}
