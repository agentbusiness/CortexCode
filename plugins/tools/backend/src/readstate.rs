//! Per-session read state: which versions of which files a session has seen, plus the line map from
//! the session's last view to the current file through its own edits (for anchor relocation and
//! the read-before-overwrite guard), and which skills were attached to the session's tool results
//! by diagnostic code ([`crate::codeskills`]).

use crate::hashedit::{Shown, View};
use cc_protocol::hashline;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileState {
    /// Content hashes of every version this session read or produced.
    seen: HashSet<String>,
    /// The session's view and the hash of the file version it maps into.
    view: Option<StoredView>,
    /// The session changed this file (an edit or a whole write).
    #[serde(default)]
    changed: bool,
    /// The session read the file since its last edit of it: that read is in the conversation, and
    /// the next edit result says it is older (see [`ReadState::take_read_since_edit`]).
    #[serde(default)]
    read_since_edit: bool,
    /// Every anchor ([`Shown::key`]) of every version of the file the session read or produced:
    /// what an anchor must be among for its line to be looked for by tag alone.
    #[serde(default)]
    shown: HashSet<u64>,
    /// The content hash of the version the session's last result showed whole, when it did.
    #[serde(default)]
    whole_shown: Option<String>,
}

/// Most anchors kept per file and session. Past it the older ones go and the newest version's
/// stay: an anchor from a version that far back is refused as stale and read again.
const SHOWN_MAX: usize = 60_000;

impl FileState {
    /// The session has this version of the file in a result: its anchors are known to it.
    fn show(&mut self, lines: &[String]) {
        if self.shown.len() + lines.len() > SHOWN_MAX {
            self.shown.clear();
        }
        self.shown.extend(lines.iter().enumerate().map(|(index, line)| Shown::key(index + 1, &hashline::tag_bytes(line))));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredView {
    base_lines: Vec<String>,
    map: Vec<Option<usize>>,
    current_hash: String,
}

#[derive(Default)]
pub struct ReadState {
    sessions: Mutex<HashMap<String, HashMap<PathBuf, FileState>>>,
    /// Skill URIs whose text a session's tool results already carried, by session.
    skills: Mutex<HashMap<String, BTreeSet<String>>>,
    /// One-time notes a session's results already carried (e.g. that it has no Python venv).
    notes: Mutex<HashMap<String, HashSet<String>>>,
}

impl ReadState {
    fn with<R>(&self, f: impl FnOnce(&mut HashMap<String, HashMap<PathBuf, FileState>>) -> R) -> R {
        let mut guard = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// The session read (all or part of) this version of the file.
    pub fn record_read(&self, session: &str, path: &Path, hash: &str, lines: &[String]) {
        self.with(|sessions| {
            let state = sessions.entry(session.to_owned()).or_default().entry(path.to_path_buf()).or_default();
            state.read_since_edit = true;
            state.seen.insert(hash.to_owned());
            state.show(lines);
            state.whole_shown = None;
            state.view =
                Some(StoredView { base_lines: lines.to_vec(), map: (0..lines.len()).map(Some).collect(), current_hash: hash.to_owned() });
        });
    }

    /// The session's view, valid only if the file is still exactly the version it maps into.
    pub fn view(&self, session: &str, path: &Path, current_hash: &str) -> Option<View> {
        self.with(|sessions| {
            let stored = sessions.get(session)?.get(path)?.view.as_ref()?;
            (stored.current_hash == current_hash).then(|| View { base_lines: stored.base_lines.clone(), map: stored.map.clone() })
        })
    }

    /// What the session was shown of the file over time, for the file as it is now (`current_hash`).
    pub fn shown(&self, session: &str, path: &Path, current_hash: &str) -> Shown {
        self.with(|sessions| {
            sessions.get(session).and_then(|files| files.get(path)).map_or_else(Shown::default, |state| Shown {
                anchors: state.shown.clone(),
                whole: state.whole_shown.as_deref() == Some(current_hash),
            })
        })
    }

    /// The session changed the file from `old_hash` to `new_hash` with this line map; `lines` are
    /// the new version's, and `whole` says the edit's result shows all of them.
    pub fn record_edit(&self, session: &str, path: &Path, old_hash: &str, new_hash: &str, line_map: Option<&[Option<usize>]>, lines: &[String], whole: bool) {
        self.with(|sessions| {
            let state = sessions.entry(session.to_owned()).or_default().entry(path.to_path_buf()).or_default();
            state.seen.insert(new_hash.to_owned());
            state.changed = true;
            state.show(lines);
            state.whole_shown = whole.then(|| new_hash.to_owned());
            state.view = match (state.view.take(), line_map) {
                (Some(stored), Some(step)) if stored.current_hash == old_hash => {
                    let view = View { base_lines: stored.base_lines, map: stored.map }.compose(step);
                    Some(StoredView { base_lines: view.base_lines, map: view.map, current_hash: new_hash.to_owned() })
                }
                _ => None,
            };
        });
    }

    /// The session wrote the whole file (it knows every line of the new version).
    pub fn record_write(&self, session: &str, path: &Path, new_hash: &str, lines: &[String]) {
        self.record_read(session, path, new_hash, lines);
        self.with(|sessions| {
            let state = sessions.entry(session.to_owned()).or_default().entry(path.to_path_buf()).or_default();
            state.changed = true;
            state.read_since_edit = false;
        });
    }

    /// Whether the session read the file since its last edit of it; reset (asked once per edit).
    pub fn take_read_since_edit(&self, session: &str, path: &Path) -> bool {
        self.with(|sessions| {
            sessions.get_mut(session).and_then(|files| files.get_mut(path)).is_some_and(|state| std::mem::take(&mut state.read_since_edit))
        })
    }

    /// Whether the session already changed this file (an edit or a whole write).
    pub fn has_changed(&self, session: &str, path: &Path) -> bool {
        self.with(|sessions| sessions.get(session).and_then(|files| files.get(path)).is_some_and(|state| state.changed))
    }

    pub fn has_seen(&self, session: &str, path: &Path, hash: &str) -> bool {
        self.with(|sessions| sessions.get(session).and_then(|files| files.get(path)).is_some_and(|state| state.seen.contains(hash)))
    }

    pub fn has_read_any(&self, session: &str, path: &Path) -> bool {
        self.with(|sessions| sessions.get(session).and_then(|files| files.get(path)).is_some_and(|state| !state.seen.is_empty()))
    }

    fn with_skills<R>(&self, f: impl FnOnce(&mut HashMap<String, BTreeSet<String>>) -> R) -> R {
        let mut guard = match self.skills.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Whether a tool result of the session already carried the text of this skill.
    pub fn skill_attached(&self, session: &str, uri: &str) -> bool {
        self.with_skills(|skills| skills.get(session).is_some_and(|attached| attached.contains(uri)))
    }

    /// A tool result of the session carried the text of this skill.
    pub fn record_skill(&self, session: &str, uri: &str) {
        self.with_skills(|skills| skills.entry(session.to_owned()).or_default().insert(uri.to_owned()));
    }

    /// True the first time `key` is asked for in `session` (a note said once per session).
    pub fn first_note(&self, session: &str, key: &str) -> bool {
        let mut guard = match self.notes.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.entry(session.to_owned()).or_default().insert(key.to_owned())
    }

    pub fn export_skills(&self) -> serde_json::Value {
        self.with_skills(|skills| serde_json::to_value(&*skills).unwrap_or(serde_json::Value::Null))
    }

    pub fn import_skills(&self, value: serde_json::Value) {
        let Ok(parsed) = serde_json::from_value::<HashMap<String, BTreeSet<String>>>(value) else { return };
        self.with_skills(|skills| {
            for (session, attached) in parsed {
                skills.entry(session).or_default().extend(attached);
            }
        });
    }

    pub fn export(&self) -> serde_json::Value {
        self.with(|sessions| {
            let as_strings: HashMap<&String, HashMap<String, &FileState>> = sessions
                .iter()
                .map(|(session, files)| (session, files.iter().map(|(path, state)| (path.to_string_lossy().into_owned(), state)).collect()))
                .collect();
            serde_json::to_value(as_strings).unwrap_or(serde_json::Value::Null)
        })
    }

    pub fn import(&self, value: serde_json::Value) {
        let Ok(parsed) = serde_json::from_value::<HashMap<String, HashMap<String, FileState>>>(value) else { return };
        self.with(|sessions| {
            for (session, files) in parsed {
                let entry = sessions.entry(session).or_default();
                for (path, state) in files {
                    entry.insert(PathBuf::from(path), state);
                }
            }
        });
    }
}
