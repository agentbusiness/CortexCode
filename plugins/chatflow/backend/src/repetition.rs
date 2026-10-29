//! Structural repetition stop for the agent loop (no output caps; loops end here instead).
//!
//! Mechanical rules, not judgements:
//! - the same tool call (normalized name and canonical arguments) returning byte-identical output
//!   for the `repeat_at`-th time (data, shipped as 3) since the last correction is a repetition;
//! - the same failing contract report for the third time is a repetition;
//! - a generation the model plugin stopped as repeating itself (`finish_reason = repeated`) is one.
//! - the same command (its first command, without output filters and redirections) ending with the
//!   same exit status for the `repeat_at`-th time with no edit in between is a rerun: told once
//!   per edit-free stretch that it cannot show anything new (seen: a passing test run again eight
//!   times, each with another `| grep`), never a stop.
//!
//! The first repetition earns a corrective replanning message; a repetition after that stops the
//! run with an honest status. A repeated read never stops it: reading a file again changes
//! nothing and costs one call, while stopping ends work that may be close to done (seen: a task
//! stopped mid-fix for reading its own file twice). It earns the correction once.

use std::collections::HashMap;

/// The model's read tool (files, folders, URIs).
const READ: &str = "read";
/// The model's command tool.
const RUN: &str = "run";

/// The command a `run` call is about: its first command that is not `cd`, `echo` or `sleep`,
/// without what follows a pipe and without redirections (`cargo test x 2>&1 | grep ok` and
/// `echo ==; cargo test x > log` are both `cargo test x`). Mechanical; empty when none.
pub fn base_command(command: &str) -> String {
    command
        .split(|c| c == ';' || c == '\n')
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .map(|part| part.split('|').next().unwrap_or("").split_whitespace().take_while(|word| !word.contains('>') && !word.starts_with('<')).collect::<Vec<_>>().join(" "))
        .find(|part| !part.is_empty() && !["cd", "echo", "sleep", "printf"].contains(&part.split(' ').next().unwrap_or("")))
        .unwrap_or_default()
}

/// How a command's output ended: its last `[exit …]`, timeout or no-status line (the tools
/// plugin's), or the whole output when it has none.
fn ending(output: &str) -> u64 {
    let last = output.lines().rev().find(|line| line.starts_with('[')).unwrap_or(output);
    // `[exit 0, 12s]` and `[exit 0]` end the same way.
    fingerprint(last.split(',').next().unwrap_or(last))
}

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
    /// The same read with the same result for the `READ_REPEAT_AT`-th time: told once that the
    /// content is above (a reread is harmless; it never escalates or stops the run).
    Reread(String),
    /// The same command with the same exit status for the `repeat_at`-th time with no edit in
    /// between: told once per edit-free stretch (never escalates or stops the run).
    Rerun(String),
}

/// Identical reads (same target, same content) before the model is told it has them already.
const READ_REPEAT_AT: u32 = 3;

#[derive(Debug)]
pub struct Detector {
    seen: HashMap<(String, u64), u32>,
    corrected: bool,
    /// A repeated read was corrected already (it is told once; reads never stop the run).
    read_corrected: bool,
    /// How many identical (call, result) pairs make a repetition (data: `limits.repeat_at`).
    repeat_at: u32,
    /// Base command and exit status of the runs since the last edit, with their count.
    runs: HashMap<(String, u64), u32>,
}

impl Detector {
    pub fn new(repeat_at: u32) -> Self {
        Self { seen: HashMap::new(), corrected: false, read_corrected: false, repeat_at: repeat_at.max(2), runs: HashMap::new() }
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

    /// Record one executed tool call and its output: a read counts toward the correction only.
    pub fn observe_call(&mut self, tool: &str, args: &serde_json::Value, output: &str) -> Verdict {
        let signature = Self::signature(tool, args);
        if tool == RUN {
            let exact = self.observe(&signature, output);
            if exact != Verdict::Fine {
                return exact;
            }
            let base = base_command(args.get("command").and_then(serde_json::Value::as_str).unwrap_or(""));
            if base.is_empty() {
                return Verdict::Fine;
            }
            let count = self.runs.entry((base.clone(), ending(output))).or_insert(0);
            *count += 1;
            if *count == self.repeat_at {
                return Verdict::Rerun(format!("`{}`", base.chars().take(300).collect::<String>()));
            }
            return Verdict::Fine;
        }
        if tool != READ {
            return self.observe(&signature, output);
        }
        let key = (signature.clone(), fingerprint(output));
        let count = self.seen.entry(key).or_insert(0);
        *count += 1;
        if *count < self.repeat_at.max(READ_REPEAT_AT) {
            return Verdict::Fine;
        }
        self.seen.retain(|(seen, _), _| seen != &signature);
        if self.read_corrected {
            return Verdict::Fine;
        }
        self.read_corrected = true;
        Verdict::Reread(format!("`{}`", signature.chars().take(300).collect::<String>()))
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
        self.runs.clear();
        self.corrected = false;
        self.read_corrected = false;
    }

    /// The workspace changed (an edit, a command that wrote files): the same call returning the
    /// same output after it is a fix that did not work, not a loop; counting starts again. The
    /// correction already given stands.
    pub fn changed(&mut self) {
        self.seen.clear();
        self.runs.clear();
    }
}
