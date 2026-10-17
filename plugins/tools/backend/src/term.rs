//! The `terminal` tool: interactive programs in a real terminal (the terminal plugin's PTYs, which
//! the user also sees in the Terminal tab). `run` covers commands that finish on their own; a REPL,
//! an editor, a prompt that waits for input or a program that redraws the screen needs keys and a
//! screen. A project that runs in a container gets its terminals inside the container.
//!
//! Keys are text with tmux-style names in angle brackets for the keys text cannot carry:
//! `<Enter>`, `<Tab>`, `<Esc>`, `<BS>`, `<Del>`, `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<Home>`,
//! `<End>`, `<PgUp>`, `<PgDn>`, `<Space>`, `<C-x>` (control + a letter or one of `[\]^_@`) and
//! `<lt>` for a literal `<`. Anything else in angle brackets is sent as written.

use regex::Regex;
use std::time::{Duration, Instant};

/// The bytes a key string sends.
pub fn keys_to_bytes(keys: &str) -> String {
    let mut out = String::with_capacity(keys.len());
    let mut rest = keys;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        match after.find('>').map(|close| (&after[1..close], close)) {
            Some((name, close)) if named_key(name).is_some() => {
                out.push_str(named_key(name).unwrap_or_default());
                rest = &after[close + 1..];
            }
            Some((name, close)) if control_key(name).is_some() => {
                out.push(control_key(name).unwrap_or('\0'));
                rest = &after[close + 1..];
            }
            _ => {
                out.push('<');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_key(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "cr" | "return" => "\r",
        "tab" => "\t",
        "esc" | "escape" => "\x1b",
        "bs" | "backspace" => "\x7f",
        "del" | "delete" => "\x1b[3~",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pgup" | "pageup" => "\x1b[5~",
        "pgdn" | "pagedown" => "\x1b[6~",
        "space" => " ",
        "lt" => "<",
        _ => return None,
    })
}

/// `C-c` -> ETX (0x03): control clears the two high bits of the key's ASCII code.
fn control_key(name: &str) -> Option<char> {
    let key = name.strip_prefix("C-").or_else(|| name.strip_prefix("c-"))?;
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let upper = c.to_ascii_uppercase();
    matches!(upper, 'A'..='Z' | '@' | '[' | '\\' | ']' | '^' | '_').then(|| char::from((upper as u8) & 0x1f))
}

/// When to return the screen after sending keys.
pub struct Wait {
    /// Return as soon as the screen matches this.
    pub until: Option<Regex>,
    /// Otherwise return once the screen has not changed for this long.
    pub settle: Duration,
    /// Never wait longer than this.
    pub limit: Duration,
}

/// What a poll saw.
#[derive(Debug, PartialEq)]
pub enum Settled {
    Matched,
    Quiet,
    Limit,
    Exited,
}

/// Track screens until the wait is over. `observe` is given every new screen with its time.
pub struct Watcher {
    wait: Wait,
    started: Instant,
    last_change: Instant,
    last: Option<String>,
}

impl Watcher {
    pub fn new(wait: Wait, now: Instant) -> Self {
        Self { wait, started: now, last_change: now, last: None }
    }

    /// Feed one screen (text plus cursor) and whether the program is still alive; Some when done.
    pub fn observe(&mut self, screen: &str, alive: bool, now: Instant) -> Option<Settled> {
        if self.last.as_deref() != Some(screen) {
            self.last = Some(screen.to_owned());
            self.last_change = now;
        }
        if let Some(until) = &self.wait.until
            && until.is_match(screen)
        {
            return Some(Settled::Matched);
        }
        if !alive {
            return Some(Settled::Exited);
        }
        if now.duration_since(self.started) >= self.wait.limit {
            return Some(Settled::Limit);
        }
        // Without a pattern, a screen that stopped changing is the answer.
        if self.wait.until.is_none() && now.duration_since(self.last_change) >= self.wait.settle {
            return Some(Settled::Quiet);
        }
        None
    }
}

/// The screen as the model reads it, with `▌` inserted where the cursor is (before the character it
/// is on), so the model knows where typing goes.
pub fn render(text: &str, cursor_row: u16, cursor_col: u16) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let row = cursor_row as usize;
    while lines.len() <= row {
        lines.push(String::new());
    }
    let line = &mut lines[row];
    let col = cursor_col as usize;
    let chars: Vec<char> = line.chars().collect();
    let mut marked: String = chars.iter().take(col).collect();
    while marked.chars().count() < col {
        marked.push(' ');
    }
    marked.push('▌');
    marked.extend(chars.iter().skip(col));
    *line = marked;
    while lines.last().is_some_and(|l| l.trim().is_empty()) && lines.len() > row + 1 {
        lines.pop();
    }
    lines.join("\n")
}
