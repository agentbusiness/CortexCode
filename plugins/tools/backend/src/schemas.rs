//! Model-facing tool specs and the usage guide. Both are stable for the life of a session (changing
//! them would invalidate the prompt cache); only the edit format chosen per context varies.

use cc_protocol::contracts::model::ToolSpec;
use cc_protocol::contracts::tools::SchemasResult;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditFormat {
    Hashline,
    Replace,
    Whole,
}

impl EditFormat {
    pub fn parse(text: Option<&str>) -> EditFormat {
        match text.map(str::trim) {
            Some("replace") | Some("str_replace") => EditFormat::Replace,
            Some("whole") | Some("write") => EditFormat::Whole,
            _ => EditFormat::Hashline,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            EditFormat::Hashline => "hashline",
            EditFormat::Replace => "replace",
            EditFormat::Whole => "whole",
        }
    }
}

fn intent() -> Value {
    json!({"type": "string", "description": "What this call is for, in a few words (the user sees it)."})
}

/// Every URI `read` takes, with what it gives. The one place they are listed for the model.
const READ_PATH: &str = "What to read: a file or folder relative to the workspace (a file may end in a line range: src/app.ts:120-180, or name one definition: src/app.ts#Cart::total), or one of these.\n\
Find code by meaning: graph://search/<words>.\n\
One function or class, by its name: symbol://<name> (its code, what it takes and returns), graph://used-by/<name> (every place that uses it), graph://uses/<name> (everything it uses), graph://impact/<name> (what a change to it reaches), graph://path/<name>/<name> (how two relate), outline://<path> (what a file contains).\n\
Language server, at path:line:col: definition://<path:line:col>, references://<path:line:col> (every use), hover://<path:line:col> (type and docs), diagnostics://<path> (errors; diagnostics:// for every file).\n\
Host: artifact://<id> (a saved long output; may end in :range), task://<id> and agent://<name> (background agents and their tasks), board:// (messages between agents), schedule:// (routines: what runs on a clock), rule:// (project rules), skill://<plugin>/<name> (a how-to).\n\
{online}\n\
Another repository, read-only (<project>: its name or path): <project>:<path>, graph://<project>/search/<words>, symbol://<project>/<name>, outline://<project>/<path>.";

/// The online reads, or (offline) only the local notes: an offline project is not shown a fetch it
/// cannot use (seen: a model trying a PR's URL three ways in a workspace without network).
fn read_path(offline: bool) -> String {
    let online = if offline {
        "Research notes (local): research://notes/<topic>."
    } else {
        "Online: research://search/<words>, research://fetch/<url> (a page or PDF as text), research://notes/<topic>."
    };
    READ_PATH.replace("{online}", online)
}

pub fn read_spec(offline: bool) -> ToolSpec {
    ToolSpec {
        name: "read".to_owned(),
        // Online reads named in the description: a model that wants to go online picks its tool by
        // what the tools say they do (seen: a call named `research` with read's arguments).
        description: if offline {
            "See a file or folder, or find code by meaning or name. For exact text, use run with grep -rn."
        } else {
            "See a file or folder, find code by meaning or name, search online. Exact text: run grep -rn."
        }
        .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "intent": intent(),
                "path": {"type": "string", "description": read_path(offline)},
                "range": {"type": "string", "description": "Lines to read: 50 (from line 50 on), 50-120, 50+30 (30 lines from line 50), or 5-16,40-52. Without a range a long file comes back as an outline of its definitions."}
            },
            "required": ["intent", "path"]
        }),
    }
}

pub fn edit_spec(format: EditFormat) -> ToolSpec {
    let path = json!({"type": "string", "description": "File to change, relative to the workspace; a one-off probe: `tmp/<name>`."});
    let content = json!({"type": "string", "description": "The whole text of a new file or a file of empty stubs, without read's N:hh| prefixes. An existing file is changed by `edits`, only where it needs changing."});
    let (description, properties) = match format {
        EditFormat::Hashline => (
            "Change a read file by N:hh anchors, at most two top-level definitions per call. New file: `content`.",
            json!({
                "intent": intent(),
                "path": path,
                "edits": {
                    "type": "array",
                    "description": "The changes to one or two top-level definitions of the file (a class with its methods is one): checked against the current file and applied together, or none is. If the file changed since your read, nothing is written and you get its current lines with fresh anchors. E.g. {\"op\":\"replace\",\"at\":\"12:ab\",\"to\":\"14:cd\",\"text\":\"...\"}.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["replace", "insert_after", "insert_before", "delete"], "description": "replace or delete lines at..to; insert text after or before line at."},
                            "at": {"type": "string", "description": "Anchor of the (first) line. \"0\" means before line 1 (insert_after only)."},
                            "to": {"type": "string", "description": "Anchor of the last line of a range (replace, delete); leave out for one line."},
                            "text": {"type": "string", "description": "The new lines, without N:hh| prefixes (replace, insert_after, insert_before)."}
                        },
                        "required": ["op", "at"]
                    }
                },
                "content": content
            }),
        ),
        EditFormat::Replace => (
            "Change a read file by exact text, at most two top-level definitions per call. New file: `content`.",
            json!({
                "intent": intent(),
                "path": path,
                "old": {"type": "string", "description": "Exact text to replace, without read's N:hh| prefixes. It must match the file exactly once, whitespace included (or set all)."},
                "new": {"type": "string", "description": "Replacement text."},
                "all": {"type": "boolean", "description": "Replace every occurrence."},
                "content": content
            }),
        ),
        EditFormat::Whole => (
            "Write one whole file: its complete new text.",
            json!({"intent": intent(), "path": path, "content": content}),
        ),
    };
    let required = match format {
        EditFormat::Whole => json!(["intent", "path", "content"]),
        _ => json!(["intent", "path"]),
    };
    ToolSpec {
        name: "edit".to_owned(),
        description: description.to_owned(),
        parameters: json!({"type": "object", "properties": properties, "required": required}),
    }
}

pub fn terminal_spec() -> ToolSpec {
    ToolSpec {
        name: "terminal".to_owned(),
        description: "Only for a program that waits for input or redraws (REPL, editor, ssh). Everything else: run.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "intent": intent(),
                "action": {
                    "type": "string",
                    "enum": ["open", "send", "read", "close"],
                    "description": "open starts one (your shell, or `program`) and returns its id and screen; send types `keys` and returns the screen once it stops changing (or matches `until`); read returns the screen (▌ marks the cursor); close ends it. The user sees these terminals."
                },
                "id": {"type": "string", "description": "The id open returned (send, read, close)."},
                "keys": {"type": "string", "description": "What to type (send): text plus <Enter>, <Tab>, <Esc>, <BS>, <Up>, <Down>, <Left>, <Right> and <C-c> (any <C-letter>); <lt> types `<`. E.g. \"python3<Enter>\", \"<C-c>\"."},
                "until": {"type": "string", "description": "Return as soon as the screen matches this regex (e.g. a prompt)."},
                "wait_ms": {"type": "integer", "description": "Longest wait for the screen (default 10000, at most 600000)."},
                "program": {"type": "string", "description": "open: a command line to run instead of your shell (e.g. `git rebase -i HEAD~3`); the terminal ends when it exits."}
            },
            "required": ["intent", "action"]
        }),
    }
}

pub fn run_spec() -> ToolSpec {
    ToolSpec {
        name: "run".to_owned(),
        description: "Run bash: build, test, grep, scripts. Project files: read with read, change with edit; no cat, sed.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "intent": intent(),
                "command": {"type": "string", "description": "The command line. Every run starts in the repository root: a cd lasts for this command only, exports carry over. A failing test run shows its counts, one line per failing test and the first failure whole; the whole output is artifact://<id>. Long output is saved whole as artifact://<id>: you see its head and tail and can read any range of it."},
                "timeout_ms": {"type": "integer", "description": "Kill the command and everything it started after this many milliseconds (default 600000)."},
                "background": {"type": "boolean", "description": "Start as a background job (servers, watchers, long builds) and return at once; you are woken when it exits."},
                "wake": {
                    "type": "object",
                    "description": "Background jobs: also wake you when output matches `pattern` (regex) or after `silence_s` seconds without output.",
                    "properties": {"pattern": {"type": "string", "description": "Regex in rolling 64 KiB output windows; full output stays in the artifact."}, "silence_s": {"type": "number"}}
                },
                "cwd": {"type": "string", "description": "Run this one command in another directory."},
                "permission": {"type": "string", "description": "Only for an install, another write outside the workspace, or a program the sandbox cannot start: why this command needs it, e.g. \"install the image library the thumbnails need\". Commands may write only in the workspace, temp dirs, package caches and your Python venv (python and pip use it, so `pip install` needs no flags). An install changes the user's machine, so the user is asked; once approved the command runs with writes anywhere. Never for what commands may do already."}
            },
            "required": ["intent", "command"]
        }),
    }
}

/// What spans the tools: which tool for what, and how each result reads; each tool's own
/// mechanics are in its description (said once, there).
pub fn guide(format: EditFormat, offline: bool) -> String {
    let anchors = match format {
        EditFormat::Hashline => {
            "  N:hh (the line number and a short hash of the line) is the line's anchor: edit names lines by anchor, never by bare line number, and an anchor stops matching once its line changes."
        }
        EditFormat::Replace | EditFormat::Whole => "  The N:hh| prefix is not part of the file: leave it out of what you send to edit.",
    };
    let change = match format {
        EditFormat::Hashline => "each function it changed, whole, as it is now (other code: the changed lines and 2 around them), with new anchors; then any lines it removed",
        EditFormat::Replace | EditFormat::Whole => "each function it changed, whole, as it is now (other code: the changed lines and 2 around them); then any lines it removed",
    };
    // Local first: the project answers most questions, and the network is used only when it cannot
    // (seen: a request naming a pull request sent the model to the network before the code).
    let network = if offline {
        "This project has no network: commands cannot reach the internet, research://search and research://fetch do not answer, and nothing can be installed or downloaded. To find out how something works, use the project's files."
    } else {
        "To find out how something works (the code, an API, a tool), look in the project first: its files, docs and tests, graph://search. Go online only when the project cannot answer: read research://search/<words>, then read research://fetch/<url> for the page that answers it."
    };
    let read_for = if offline {
        "to see a file or folder, and to find code by meaning or by name (below)"
    } else {
        "to see a file or folder, to find code by meaning or by name (below), and to search and read online (research://search/<words>, research://fetch/<url>)"
    };
    let (how, fresh) = match format {
        EditFormat::Hashline => ("by anchors", ", with new anchors"),
        EditFormat::Replace | EditFormat::Whole => ("by exact text (old, new)", ""),
    };
    format!(
        "Which tool:\n\
1. read: {read_for}.\n\
2. edit: to change or write a file (below).\n\
3. run: to build, test, run a script, list files or search exact text (`grep -rn`). cat, head, tail, sed and here-documents on project files are blocked in run: read a project file with read, change it with edit. Checks of your work are tests in the project (a project without tests gets them), run whole: no `| tail` or `| grep`, the result shows each failure; a one-off try (call a function, print a value) is a file in `tmp/`, run from the project.\n\
4. terminal: only for a program that waits for input or redraws the screen. Everything else: run.\n\
5. An install, another write outside the workspace, or a program the sandbox cannot start (\"Operation not permitted\"): run the command that does it with `permission` saying why. An install changes the user's machine, so the user is asked first; ask only when the task needs it. Never change the user's shell setup or tool settings (dotfiles, global config) to get a command through.\n\
6. {network}\n\
7. Git: a commit or push request is permission for its normal steps, including staging. Use run without `permission`; do not ask again. Only commit or push when requested. Destructive actions (discard edits, delete refs, rewrite history, force push) get a separate confirmation before running.\n\
Give every call a short `intent` first.\n\
\n\
Working with files:\n\
1. Read a file before you change it: its lines come numbered (N:hh|). A long file shows its definitions; read one of them with `path#Name`.\n\
2. Change an existing file with edit {how}, one or two top-level definitions per call (a class with its methods is one). Never rewrite, reformat or clean up code that works, and never write a file with working code whole again: code the task does not touch stays exactly as it is, unless the user asks to change it.\n\
3. The result shows each changed function as it is now{fresh}: your next edit uses that. Read the file again only after a command changed it.\n\
4. Write whole with `content` only a new file, a file of empty stubs, or a probe in the scratch folder `tmp/` (`tmp/<name>`, as often as you like; git ignores it).\n\
5. A refused edit wrote nothing: its message shows the current lines and what to send instead.\n\
6. Keep every existing name (function, type, field, variable, module, file): other code and other agents' tasks use it. Rename, move or delete one only when the task asks for it, and then change every use listed by graph://used-by/<name> in the same turn.\n\
7. Every edit and command is recorded and can be undone: `cx changed` lists your steps with their numbers, `cx undo --step N` puts back what step N changed. So try the change and run the check; a try that went wrong is one undo, not a reason to read more first.\n\
\n\
How results read:\n\
- A file: `path:` and then its lines as N:hh|text.\n{anchors}\n  A long file shows only its definitions; ⋮ N-M marks lines left out: read path:N-M to see them.\n\
- A folder: one entry per line; a name ending in / is a folder.\n\
- edit: {change}, then the file's errors, or `No errors.` and the next step.\n\
- run: the command's output, then `[exit N]`: exit 0 means it succeeded. A long output is cut in the middle: `[N lines not shown: read artifact://<id>:A-B for them]`.\n\
- A code result: one line, `path:line  signature — what it does`.\n\
- A call result: `path:line  in <function>: <calling line>` (used-by) or `path:line  calls <function>: <calling line>` (uses).\n\
- A failure says what went wrong; its last sentence is the next step.\n\
\n\
Find code:\n\
- By meaning: read graph://search/<words>. When nothing fits, search again with other words.\n\
- From a result, by its name:\n  its code, what it takes and returns: symbol://<name>\n  exactly where it is used, from the code graph, not a search: graph://used-by/<name>\n  exactly what it uses: graph://uses/<name>\n  what is around it in its file: outline://<path>\n  what a change to it reaches (callers, tests): graph://impact/<name>\n  how two relate (the shortest chain between them): graph://path/<name>/<name>\n\
- Before you change what a function takes or returns, read graph://used-by/<name>: every call listed there changes with it.\n\
\n\
`cx` (on PATH, through run) reaches background agents, their tasks, the board, the code graph, the browser and CortexCode's own plugins; `cx tools` lists its commands, and `cx research deep <topic>` runs a cited research job. To change how CortexCode itself works, edit its skills, prompts and texts in `$CORTEX_HOME/live/plugins/<id>/` (`data/` or `backend/data/`): an edit is live at once.\n"
    )
}

/// The tool specs and guide for a workspace (`offline`: it holds the offline marker).
pub fn schemas(format: EditFormat, offline: bool) -> SchemasResult {
    SchemasResult {
        tools: vec![read_spec(offline), edit_spec(format), run_spec(), terminal_spec()],
        versions: json!({"read": "read@1", "edit": format!("edit-{}@1", format.name()), "run": "run@1", "terminal": "terminal@1"}),
        guide: guide(format, offline),
    }
}
