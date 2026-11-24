//! Structural repetition stop for the agent loop (no output caps; loops end here instead).
//!
//! Mechanical rules, not judgements:
//! - the same tool call (normalized name and canonical arguments) returning byte-identical output
//!   for the `repeat_at`-th time (data, shipped as 3) since the last correction is a repetition;
//! - the same failing contract report for the third time is a repetition;
//! - a generation the model plugin stopped as repeating itself (`finish_reason = repeated`) is one.
//! - the same command ([`base_command`]: without output filters and redirections) run for the
//!   `repeat_at`-th time or more with no edit in between is a rerun, whatever its filters show and
//!   however they end (a `| grep` that matches nothing exits 1): told at each such run that it
//!   cannot show anything new (seen: a passing test run again eight times, each with another
//!   `| grep`; clippy run twenty times between two edits, each filter keyed apart by its exit
//!   status), never a stop.
//!
//! The first repetition of a call earns a corrective replanning message; that same call repeating
//! again after its correction stops the run with an honest status. Another call that repeats later
//! earns its own correction first: one correction for the whole task stopped a long task at its
//! second, unrelated repeat. A repeated read never stops it: reading a file again changes
//! nothing and costs one call, while stopping ends work that may be close to done (seen: a task
//! stopped mid-fix for reading its own file twice). It earns the correction once.

use std::collections::{HashMap, HashSet};

/// The model's read tool (files, folders, URIs).
const READ: &str = "read";
/// The model's command tool.
const RUN: &str = "run";

/// Programs that only set the scene for the command a `run` call is about.
const SETTING: [&str; 5] = ["cd", "echo", "sleep", "printf", "export"];

/// Whether a word sets a shell variable (`NAME=value`): before a command word it is a setting of
/// that command, alone it is a statement that runs nothing.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else { return false };
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The command a `run` call is about: its first command that is not `cd`, `echo`, `sleep`,
/// `printf` or `export`, without what follows a pipe, without redirections and without the
/// `NAME=value` settings before it (`cargo test x 2>&1 | grep ok`, `echo ==; cargo test x > log`
/// and `R=/repo; RUST_BACKTRACE=1 cargo test x` are all `cargo test x`). A statement that only
/// sets variables is no command.
/// A command given its script in the call itself (an option `-c` or `-e` followed by a quoted
/// script, or a here-document) is the whole call, script included: its program and option say
/// nothing about what it runs (seen: every `python3 -c "…"` of a task counted as one command, and
/// every line that began `R=/path;`; 55 of 57 rerun notes were about commands never run before).
/// Mechanical; empty when none.
pub fn base_command(command: &str) -> String {
    let statements = command.split(|c| c == ';' || c == '\n').flat_map(|part| part.split("&&")).flat_map(|part| part.split("||"));
    for statement in statements {
        let head = statement.split('|').next().unwrap_or("");
        let words: Vec<&str> = head.split_whitespace().skip_while(|word| is_assignment(word)).take_while(|word| !word.contains('>') && !word.starts_with('<')).collect();
        let Some(program) = words.first() else { continue };
        if SETTING.contains(program) {
            continue;
        }
        let script_option = words.windows(2).skip(1).any(|pair| matches!(pair[0], "-c" | "-e") && pair[1].starts_with(['"', '\'', '$']));
        if script_option || head.contains("<<") {
            return command.trim().to_owned();
        }
        return words.join(" ");
    }
    String::new()
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
    /// A call's first repetition since the run started (or since the last user input): replan.
    Replan(String),
    /// The same call repeated again after its own correction: stop.
    Stop(String),
    /// The same read with the same result for the `READ_REPEAT_AT`-th time: told once that the
    /// content is above (a reread is harmless; it never escalates or stops the run).
    Reread(String),
    /// The same command for the `repeat_at`-th time or more with no edit in between: told at each
    /// such run (never escalates or stops the run).
    Rerun(String),
}

/// Identical reads (same target, same content) before the model is told it has them already.
const READ_REPEAT_AT: u32 = 3;

#[derive(Debug)]
pub struct Detector {
    seen: HashMap<(String, u64), u32>,
    /// The calls (their signatures) that earned their correction already: one of them repeating
    /// again stops the run. Per call, so a second, unrelated repeat late in a long task gets its
    /// own correction instead of ending the task.
    corrected: HashSet<String>,
    /// A generation stopped as repeating itself was corrected already.
    text_corrected: bool,
    /// A repeated read was corrected already (it is told once; reads never stop the run).
    read_corrected: bool,
    /// How many identical (call, result) pairs make a repetition (data: `limits.repeat_at`).
    repeat_at: u32,
    /// Base command of the runs since the last edit, with their count.
    runs: HashMap<String, u32>,
    /// The workspace changed since the last reset (an edit, a command that wrote files): the
    /// work is past finding out, so a rerun's next step is an edit.
    edited: bool,
}

impl Detector {
    pub fn new(repeat_at: u32) -> Self {
        Self { seen: HashMap::new(), corrected: HashSet::new(), text_corrected: false, read_corrected: false, repeat_at: repeat_at.max(2), runs: HashMap::new(), edited: false }
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
            let count = self.runs.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count >= self.repeat_at {
                return Verdict::Rerun(format!("`{base}`"));
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
        Verdict::Reread(format!("`{signature}`"))
    }

    /// Record one executed call and its output.
    pub fn observe(&mut self, signature: &str, output: &str) -> Verdict {
        let key = (signature.to_owned(), fingerprint(output));
        let count = self.seen.entry(key).or_insert(0);
        *count += 1;
        if *count >= self.repeat_at {
            let what = format!("`{signature}`");
            // This call was corrected before and repeats again: stop. Its first time: its
            // correction, and every count starts again.
            if self.corrected.contains(signature) {
                return Verdict::Stop(what);
            }
            self.corrected.insert(signature.to_owned());
            self.seen.clear();
            return Verdict::Replan(what);
        }
        Verdict::Fine
    }

    /// The model plugin stopped a generation for repeating itself: corrected once, then a stop
    /// (its own count, apart from the calls').
    pub fn generation_repeated(&mut self) -> Verdict {
        let what = "the same text".to_owned();
        if self.text_corrected {
            Verdict::Stop(what)
        } else {
            self.text_corrected = true;
            self.seen.clear();
            Verdict::Replan(what)
        }
    }

    /// Whether the workspace changed since the last reset (the task made an edit).
    pub fn edited(&self) -> bool {
        self.edited
    }

    /// New input from the user, or a new plan task, resets the detector.
    pub fn reset(&mut self) {
        self.seen.clear();
        self.runs.clear();
        self.corrected.clear();
        self.text_corrected = false;
        self.read_corrected = false;
        self.edited = false;
    }

    /// The workspace changed (an edit, a command that wrote files): the same call returning the
    /// same output after it is a fix that did not work, not a loop; counting starts again. The
    /// corrections already given stand.
    pub fn changed(&mut self) {
        self.seen.clear();
        self.runs.clear();
        self.edited = true;
    }
}
