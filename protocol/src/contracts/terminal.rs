//! `terminal@1`: PTY-backed terminals for the UI (and readable by agents via screen snapshots).

use serde::{Deserialize, Serialize};

pub mod methods {
    /// [`OpenParams`] -> [`TermInfo`]. Runs the shell as a login shell (`-l`) on a new PTY.
    pub const OPEN: &str = "terminal.open";
    /// Write user input (UTF-8 by default; `base64: true` sends exact binary bytes).
    pub const WRITE: &str = "terminal.write";
    /// [`ResizeParams`] -> [`TermInfo`]: sets the PTY window size (the shell receives SIGWINCH).
    pub const RESIZE: &str = "terminal.resize";
    /// [`TermParams`] -> [`TermInfo`]: SIGHUP to the shell's and the foreground process groups,
    /// then SIGKILL if they do not exit; the terminal is removed from the list.
    pub const CLOSE: &str = "terminal.close";
    pub const LIST: &str = "terminal.list";
    /// Current screen as plain text (parsed by a VT emulator), for agents and tests.
    pub const SNAPSHOT: &str = "terminal.snapshot";
}

pub mod topics {
    /// [`OutputEvent`] `{term_id, data}` - raw output bytes, base64, coalesced to about every 16 ms.
    pub const OUTPUT: &str = "terminal.output";
    /// [`ExitEvent`] `{term_id, exit_code}`
    pub const EXIT: &str = "terminal.exit";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputEvent {
    pub term_id: String,
    /// Standard base64 of the raw bytes.
    pub data: String,
    /// Sequence number of this chunk within the terminal (starts at 1, no gaps).
    #[serde(default)]
    pub seq: u64,
    /// Exclusive byte offset since this PTY opened. Stable across plugin reconnects.
    /// Absent on older implementations; consumers then use the legacy snapshot fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExitEvent {
    pub term_id: String,
    pub exit_code: i32,
    /// Name of the signal that ended the process, if one did.
    #[serde(default)]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenParams {
    pub cwd: String,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Default: the user's login shell.
    #[serde(default)]
    pub shell: Option<String>,
    /// Arguments for `shell`; default `-l` (a login shell). A program that is not a shell (for
    /// example `docker` with `exec -it <container> bash -l` for a project that runs in a container)
    /// gets exactly these.
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TermInfo {
    pub term_id: String,
    pub cwd: String,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    /// The shell executable.
    #[serde(default)]
    pub shell: String,
    /// Set once the shell has exited.
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WriteParams {
    pub term_id: String,
    pub data: String,
    /// `data` is standard base64 rather than UTF-8 (legacy mouse reports, binary input).
    #[serde(default)]
    pub base64: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResizeParams {
    pub term_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TermParams {
    pub term_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResult {
    pub terminals: Vec<TermInfo>,
}

/// Request formatted state only for a terminal renderer. Agent snapshots remain plain text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotParams {
    pub term_id: String,
    #[serde(default)]
    pub restore: bool,
}

/// A terminal repaint followed by output strictly beyond `offset` (overlapping chunks are cut).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScreenRestore {
    /// Standard base64 terminal bytes: both screen buffers, cursor, input modes, pending sequence.
    pub data: String,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub term_id: String,
    /// Visible screen, one line per row (trailing blanks trimmed).
    pub text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub alive: bool,
    /// The program switched to the alternate screen (full-screen apps such as vim or less).
    #[serde(default)]
    pub alternate_screen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<ScreenRestore>,
    /// Whether this implementation understands `terminal.write {base64: true}`.
    #[serde(default)]
    pub input_base64: bool,
}

// Optional terminal@1 inspection/completion response declarations, shared with CortexTerm.
// Declarations do not advertise methods or install handlers in CortexCode.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Threat {
    /// The rule's id (`pipe-to-shell`, `fake-password-prompt`, `hidden-text`, ...).
    pub rule: String,
    /// high | medium
    pub severity: String,
    /// One plain sentence: what it would do.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScriptFetchParams {
    /// An http(s) URL.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Script {
    /// The URL it came from after redirects.
    pub url: String,
    /// The script (about the first 200 KB of it).
    pub text: String,
    /// Its whole size.
    pub bytes: u64,
    /// More than `text` holds.
    #[serde(default)]
    pub truncated: bool,
    pub threats: Vec<Threat>,
    /// Bounded static effect evidence, when this provider supports it. Never execution approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<ScriptAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScriptAnalysis {
    pub statements: u32,
    pub findings: Vec<ScriptEffect>,
    pub limits: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScriptEffect {
    pub rule: String,
    /// 1-based original script line, not an excerpt-relative position.
    pub line: u32,
    pub related_lines: Vec<u32>,
    pub explanation: String,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CompleteParams {
    pub term_id: String,
    pub line: String,
    /// Byte offset of the cursor in `line`.
    pub cursor: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Completion {
    pub value: String,
    /// command | file | dir | branch | flag | history | host | process
    pub kind: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Completions {
    /// Byte offset in `line` where the completed word starts (it is replaced by `value`).
    pub from: u32,
    /// Exclusive byte end of the word to replace. Older providers omit it (replace to cursor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u32>,
    pub items: Vec<Completion>,
    /// A whole-line suggestion from history (ghost text), when one fits the line so far.
    #[serde(default)]
    pub suggestion: Option<String>,
}
