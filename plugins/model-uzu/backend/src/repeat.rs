//! Repetition guard: ends a runaway generation instead of an output cap.
//!
//! Structural rule (no meaning is judged): generated text whose tail is one block written at least
//! [`REPEATS`] times in a row, covering at least [`MIN_SPAN`] bytes, is a decoder cycle. A cycle
//! period longer than [`LONGEST_PERIOD`] is treated as a document, not a loop. The same rule ended
//! cycling generations (decided 2026-10-12); it never fires on long output
//! that does not repeat.

pub const REPEATS: usize = 8;
pub const MIN_SPAN: usize = 512;
pub const LONGEST_PERIOD: usize = 1024;
/// How many new bytes accumulate between checks.
const CHECK_EVERY: usize = 64;

/// Whether `text` ends in a cycle (see the module docs).
pub fn repeats_itself(text: &[u8]) -> bool {
    if text.len() < MIN_SPAN {
        return false;
    }
    (1..=(text.len() / REPEATS).min(LONGEST_PERIOD)).any(|period| {
        let span = (period * REPEATS).max(MIN_SPAN);
        span <= text.len() && {
            let tail = &text[text.len() - span..];
            tail[period..] == tail[..span - period]
        }
    })
}

/// Incremental guard over one stream of text (content, reasoning or tool arguments).
#[derive(Debug, Default)]
pub struct Guard {
    /// The trailing window the rule can ever look at.
    tail: Vec<u8>,
    since_check: usize,
}

impl Guard {
    /// Feed new text; true once the stream is cycling.
    pub fn push(&mut self, text: &str) -> bool {
        self.tail.extend_from_slice(text.as_bytes());
        let window = (LONGEST_PERIOD * REPEATS).max(MIN_SPAN);
        if self.tail.len() > 2 * window {
            let excess = self.tail.len() - window;
            self.tail.drain(..excess);
        }
        self.since_check += text.len();
        if self.since_check < CHECK_EVERY {
            return false;
        }
        self.since_check = 0;
        repeats_itself(&self.tail)
    }
}
