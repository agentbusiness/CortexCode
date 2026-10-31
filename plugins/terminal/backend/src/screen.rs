//! Renderer snapshots and byte positions. Shared verbatim with CortexCode.
//! The screen and its byte counter share one mutex, so a repaint has an exact output boundary.

use base64::Engine;
use cc_protocol::contracts::terminal::ScreenRestore;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, PartialEq)]
enum Boundary {
    Ground,
    Escape,
    Csi,
    Osc,
    String,
    StringEscape,
    OscEscape,
    Argument,
}

pub struct ScreenState {
    parser: vt100::Parser,
    pub offset: u64,
    boundary: Boundary,
    pending: Vec<u8>,
    utf8_left: u8,
    overflow: bool,
    // vt100 does not serialize margins or these modes. Retain their actual terminal controls.
    margins: [Option<Vec<u8>>; 2],
    origin: [bool; 2],
    modes: std::collections::BTreeMap<String, bool>,
    cursor_style: Option<Vec<u8>>,
}

impl ScreenState {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            offset: 0,
            boundary: Boundary::Ground,
            pending: Vec::new(),
            utf8_left: 0,
            overflow: false,
            margins: [None, None],
            origin: [false, false],
            modes: std::collections::BTreeMap::new(),
            cursor_style: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            let complete_csi = self.track(byte);
            if complete_csi {
                // Keep vt100 in step before recording a control's buffer-dependent state.
                self.parser.process(&bytes[start..=index]);
                start = index + 1;
                let sequence = std::mem::take(&mut self.pending);
                self.control(&sequence);
            }
        }
        self.parser.process(&bytes[start..]);
        self.offset += bytes.len() as u64;
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let old_rows = self.parser.screen().size().0;
        self.parser.screen_mut().set_size(rows, cols);
        // Mirror vt100's region resizing in the metadata it cannot serialize itself.
        for margins in self.margins.iter_mut().flatten() {
            let body = String::from_utf8_lossy(&margins[2..margins.len() - 1]);
            let mut params = body.split(';');
            let mut top = params
                .next()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(1)
                .max(1);
            let previous = params
                .next()
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(old_rows);
            let bottom = if previous == old_rows {
                rows
            } else {
                previous.min(rows)
            };
            if top > bottom {
                top = 1;
            }
            *margins = format!("\x1b[{top};{bottom}r").into_bytes();
        }
    }

    /// Clone the screen, never switch the live emulator just to inspect its other buffer.
    pub fn restore(&self) -> Option<ScreenRestore> {
        if self.overflow {
            return None;
        }
        let active = self.parser.screen();
        let (rows, cols) = active.size();
        let mut scratch = vt100::Parser::new(rows, cols, 0);
        *scratch.screen_mut() = active.clone();
        let alternate = active.alternate_screen();
        if alternate {
            scratch.process(b"\x1b[?47l");
        }
        let normal = scratch.screen().clone();
        scratch.process(b"\x1b[?47h");
        let other = scratch.screen().clone();
        let mut data = b"\x1bc".to_vec();
        self.paint(&normal, 0, &mut data);
        // 1049 saves the normal cursor for the app's eventual exit. A normal-screen snapshot
        // uses 47 so it does not overwrite that buffer's independently saved cursor.
        data.extend(if alternate {
            &b"\x1b[?1049h"[..]
        } else {
            &b"\x1b[?47h"[..]
        });
        self.paint(&other, 1, &mut data);
        if !alternate {
            data.extend(b"\x1b[?47l");
            self.cursor(&normal, 0, &mut data);
        }
        data.extend(active.input_mode_formatted());
        for (mode, on) in &self.modes {
            data.extend(format!("\x1b[{mode}{}", if *on { 'h' } else { 'l' }).bytes());
        }
        if let Some(style) = &self.cursor_style {
            data.extend(style);
        }
        // Continue a CSI/OSC/UTF-8 character split by the reader's last chunk exactly once.
        data.extend(&self.pending);
        Some(ScreenRestore {
            data: base64::engine::general_purpose::STANDARD.encode(data),
            offset: self.offset,
        })
    }

    fn cursor(&self, screen: &vt100::Screen, buffer: usize, data: &mut Vec<u8>) {
        if let Some(margins) = &self.margins[buffer] {
            data.extend(margins);
        }
        if self.origin[buffer] {
            let top = self.margins[buffer]
                .as_ref()
                .and_then(|seq| std::str::from_utf8(&seq[2..seq.len() - 1]).ok())
                .and_then(|body| body.split(';').next()?.parse::<u16>().ok())
                .unwrap_or(1)
                .max(1);
            let (row, col) = screen.cursor_position();
            data.extend(b"\x1b[?6h");
            data.extend(format!("\x1b[{};{}H", row.saturating_sub(top - 1) + 1, col + 1).bytes());
        } else {
            data.extend(b"\x1b[?6l");
            data.extend(screen.cursor_state_formatted());
        }
        data.extend(screen.attributes_formatted());
    }

    fn paint(&self, screen: &vt100::Screen, buffer: usize, data: &mut Vec<u8>) {
        data.extend(b"\x1b[?6l\x1b[r");
        data.extend(screen.state_formatted());
        let (rows, cols) = screen.size();
        let mut saved = vt100::Parser::new(rows, cols, 0);
        *saved.screen_mut() = screen.clone();
        saved.process(b"\x1b8");
        data.extend(saved.screen().cursor_state_formatted());
        data.extend(saved.screen().attributes_formatted());
        data.extend(b"\x1b7");
        self.cursor(screen, buffer, data);
    }

    fn control(&mut self, sequence: &[u8]) {
        if sequence == b"\x1bc" {
            self.margins = [None, None];
            self.origin = [false, false];
            self.modes.clear();
            self.cursor_style = None;
            return;
        }
        let Some((&last, body)) = sequence.strip_prefix(b"\x1b[").and_then(|s| s.split_last())
        else {
            return;
        };
        if last == b'r' && body.iter().all(|b| b.is_ascii_digit() || *b == b';') {
            self.margins[usize::from(self.parser.screen().alternate_screen())] =
                Some(sequence.to_vec());
        } else if last == b'q' && body.last() == Some(&b' ') {
            self.cursor_style = Some(sequence.to_vec());
        } else if matches!(last, b'h' | b'l') {
            let private = body.first() == Some(&b'?');
            let params = if private { &body[1..] } else { body };
            for param in params.split(|b| *b == b';') {
                let Ok(mode) = std::str::from_utf8(param) else {
                    continue;
                };
                if private && mode == "6" {
                    self.origin[usize::from(self.parser.screen().alternate_screen())] =
                        last == b'h';
                }
                // Only persistent terminal modes, never buffer switches or destructive controls.
                if (private && matches!(mode, "7" | "1004" | "1007" | "1015" | "1016" | "2004"))
                    || (!private && mode == "4")
                {
                    self.modes.insert(
                        format!("{}{mode}", if private { "?" } else { "" }),
                        last == b'h',
                    );
                }
                // vt100 supports 47/1049 but not 1047/1048. Normalize just its local parser.
                if private && mode == "1047" {
                    self.parser.process(if last == b'h' {
                        &b"\x1b[?47h"[..]
                    } else {
                        &b"\x1b[?47h\x1b[2J\x1b[?47l"[..]
                    });
                } else if private && mode == "1048" {
                    self.parser
                        .process(if last == b'h' { b"\x1b7" } else { b"\x1b8" });
                }
            }
        }
    }

    /// Keep only an unfinished sequence; complete OSC/DCS payloads never enter snapshots.
    fn track(&mut self, byte: u8) -> bool {
        if self.boundary == Boundary::Ground && self.utf8_left == 0 {
            self.pending.clear();
            self.overflow = false;
        }
        if self.pending.len() < 65536 {
            self.pending.push(byte);
        } else {
            self.overflow = true;
        }
        let mut csi = false;
        if matches!(byte, 0x18 | 0x1a) {
            self.pending.clear();
            self.utf8_left = 0;
            self.boundary = Boundary::Ground;
            self.overflow = false;
            return false;
        }
        self.boundary = match self.boundary {
            Boundary::Ground => {
                if self.utf8_left > 0 && byte & 0xc0 == 0x80 {
                    self.utf8_left -= 1;
                } else {
                    self.utf8_left = match byte {
                        0xc2..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf4 => 3,
                        _ => 0,
                    };
                    if byte == 0x1b {
                        return {
                            self.boundary = Boundary::Escape;
                            false
                        };
                    }
                }
                Boundary::Ground
            }
            Boundary::Escape => match byte {
                b'[' => Boundary::Csi,
                b']' => Boundary::Osc,
                b'P' | b'X' | b'^' | b'_' => Boundary::String,
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' | b' ' => {
                    Boundary::Argument
                }
                0x1b => {
                    self.pending.clear();
                    self.pending.push(byte);
                    Boundary::Escape
                }
                b'c' => {
                    csi = true;
                    Boundary::Ground
                }
                _ => Boundary::Ground,
            },
            Boundary::Csi => match byte {
                0x40..=0x7e => {
                    csi = true;
                    Boundary::Ground
                }
                0x1b => {
                    self.pending.clear();
                    self.pending.push(byte);
                    Boundary::Escape
                }
                _ => Boundary::Csi,
            },
            Boundary::Osc => match byte {
                7 => Boundary::Ground,
                0x1b => Boundary::OscEscape,
                _ => Boundary::Osc,
            },
            Boundary::String => {
                if byte == 0x1b {
                    Boundary::StringEscape
                } else {
                    Boundary::String
                }
            }
            Boundary::OscEscape => {
                if byte == b'\\' {
                    Boundary::Ground
                } else {
                    Boundary::Osc
                }
            }
            Boundary::StringEscape => {
                if byte == b'\\' {
                    Boundary::Ground
                } else {
                    Boundary::String
                }
            }
            Boundary::Argument => Boundary::Ground,
        };
        if !csi && self.boundary == Boundary::Ground && self.utf8_left == 0 {
            self.pending.clear();
        }
        csi
    }
}

impl Deref for ScreenState {
    type Target = vt100::Parser;
    fn deref(&self) -> &Self::Target {
        &self.parser
    }
}
impl DerefMut for ScreenState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.parser
    }
}
