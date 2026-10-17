//! A text file as a sequence of lines with their exact terminators, so edits rewrite only the lines
//! they touch and keep everything else byte-identical (CRLF, missing final newline, BOM).
//!
//! Line numbering and tags agree with `cc_protocol::hashline`: the file is split on `\n`, a `\r`
//! before the `\n` belongs to the terminator, and a trailing newline does not start an extra line.
//! The one deliberate difference: an empty file has zero lines here (hashline would show one empty
//! line); edits into an empty file use the start anchor `0`.

use cc_protocol::hashline;

const BOM: &str = "\u{feff}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    /// The file started with a UTF-8 byte order mark (kept on write, hidden from line 1).
    pub bom: bool,
    /// Line contents without terminators.
    pub lines: Vec<String>,
    /// Terminator of each line: `"\n"`, `"\r\n"` or `""` (only the last line may have none).
    pub eols: Vec<&'static str>,
    /// Terminator for lines this edit creates (the file's dominant style).
    pub default_eol: &'static str,
}

impl Doc {
    pub fn parse(text: &str) -> Doc {
        let (bom, body) = match text.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let mut lines = Vec::new();
        let mut eols = Vec::new();
        if !body.is_empty() {
            let mut pieces: Vec<&str> = body.split('\n').collect();
            let ends_with_newline = body.ends_with('\n');
            if ends_with_newline {
                pieces.pop();
            }
            let count = pieces.len();
            for (index, piece) in pieces.into_iter().enumerate() {
                let is_last = index + 1 == count;
                if is_last && !ends_with_newline {
                    lines.push(piece.to_owned());
                    eols.push("");
                } else if let Some(content) = piece.strip_suffix('\r') {
                    lines.push(content.to_owned());
                    eols.push("\r\n");
                } else {
                    lines.push(piece.to_owned());
                    eols.push("\n");
                }
            }
        }
        let crlf = eols.iter().filter(|eol| **eol == "\r\n").count();
        let lf = eols.iter().filter(|eol| **eol == "\n").count();
        let default_eol = if crlf > lf { "\r\n" } else { "\n" };
        Doc { bom, lines, eols, default_eol }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether the last line ends with a terminator (true for an empty file: new content gets one).
    pub fn final_newline(&self) -> bool {
        self.eols.last().is_none_or(|eol| !eol.is_empty())
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if self.bom {
            out.push_str(BOM);
        }
        for (line, eol) in self.lines.iter().zip(&self.eols) {
            out.push_str(line);
            out.push_str(eol);
        }
        out
    }

    /// Tag of line `index` (0-based).
    pub fn tag(&self, index: usize) -> String {
        hashline::tag(&self.lines[index])
    }

    /// Render 1-based inclusive lines `start..=end` as hashlines (clamped).
    pub fn render(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        let end = end.min(self.lines.len());
        let mut number = start.max(1);
        while number <= end {
            out.push_str(&hashline::render_line(number, &self.lines[number - 1]));
            out.push('\n');
            number += 1;
        }
        out
    }

    /// Rebuild from a new list of `(content, original terminator)` entries, applying the
    /// terminator rules: surviving lines keep their terminator, new lines use the dominant one, and
    /// the file keeps (or lacks) its final newline.
    pub fn rebuild(&self, entries: Vec<(String, Option<&'static str>)>) -> Doc {
        let final_newline = self.final_newline();
        let count = entries.len();
        let mut lines = Vec::with_capacity(count);
        let mut eols = Vec::with_capacity(count);
        for (index, (content, original)) in entries.into_iter().enumerate() {
            let is_last = index + 1 == count;
            let eol = match original {
                Some(eol) if !eol.is_empty() => eol,
                _ => self.default_eol,
            };
            eols.push(if is_last && !final_newline { "" } else { eol });
            lines.push(content);
        }
        Doc { bom: self.bom, lines, eols, default_eol: self.default_eol }
    }
}

/// Split model-supplied replacement text into lines: one trailing newline is a terminator, not an
/// extra empty line; `\r\n` is accepted and normalized (the file's own style is applied on write).
pub fn text_to_lines(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').map(|rest| rest.strip_suffix('\r').unwrap_or(rest)).unwrap_or(text);
    text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned()).collect()
}
