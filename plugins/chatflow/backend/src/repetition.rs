//! Structural repetition stop for the agent loop (no output caps; loops end here instead).
//!
//! Mechanical rules, not judgements:
//! - the same tool call (normalized name and canonical arguments) returning byte-identical output
//!   for the `repeat_at`-th time (data, shipped as 3) since the last correction is a repetition;
//! - the same failing contract report for the third time is a repetition;
//! - a generation the model plugin stopped as repeating itself (`finish_reason = repeated`) is one.
//!
//! The first repetition earns a corrective replanning message; a repetition after that stops the
//! run with an honest status.

use std::collections::HashMap;

/// FNV-1a 64: a stable fingerprint (mechanical; equality of bytes, not meaning).
pub fn fingerprint(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}


/// JSON text with every object's keys sorted, so equal values print equally.
pub fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", serde_json::Value::String(key.clone()), canonical(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// First repetition since the run started (or since the last user input): replan.
    Replan(String),
    /// Repeated again after a replan: stop.
    Stop(String),
}

#[derive(Debug)]
pub struct Detector {
    seen: HashMap<(String, u64), u32>,
    corrected: bool,
    /// How many identical (call, result) pairs make a repetition (data: `limits.repeat_at`).
    repeat_at: u32,
}

impl Detector {
    pub fn new(repeat_at: u32) -> Self {
        Self { seen: HashMap::new(), corrected: false, repeat_at: repeat_at.max(2) }
    }

    /// Canonical signature of a call: arguments with object keys sorted (mechanical).
    /// The call's stated intent (`intent`, `i`) is commentary, not part of what the call does, so
    /// it is left out (mechanical).
    pub fn signature(tool: &str, args: &serde_json::Value) -> String {
        let mut args = args.clone();
        if let Some(map) = args.as_object_mut() {
            map.remove("intent");
            map.remove("i");
        }
        format!("{tool} {}", canonical(&args))
    }

    /// Record one executed call and its output.
    pub fn observe(&mut self, signature: &str, output: &str) -> Verdict {
        let key = (signature.to_owned(), fingerprint(output));
        let count = self.seen.entry(key).or_insert(0);
        *count += 1;
        if *count >= self.repeat_at {
            let what = format!("`{}`", signature.chars().take(300).collect::<String>());
            return self.escalate(what);
        }
        Verdict::Fine
    }

    /// The model plugin stopped a generation for repeating itself.
    pub fn generation_repeated(&mut self) -> Verdict {
        self.escalate("the same text".to_owned())
    }

    fn escalate(&mut self, what: String) -> Verdict {
        if self.corrected {
            Verdict::Stop(what)
        } else {
            self.corrected = true;
            self.seen.clear();
            Verdict::Replan(what)
        }
    }

    /// New input from the user, or a new plan task, resets the detector.
    pub fn reset(&mut self) {
        self.seen.clear();
        self.corrected = false;
    }

    /// The workspace changed (an edit, a command that wrote files): the same call returning the
    /// same output after it is a fix that did not work, not a loop; counting starts again. The
    /// correction already given stands.
    pub fn changed(&mut self) {
        self.seen.clear();
    }
}
