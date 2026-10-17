//! What the parsers extract from one file, before cross-file resolution.
//!
//! Extraction is purely structural (tree-sitter syntax trees, manifest formats). Every rule that
//! decides what a construct *is* (a test, a route, a component, a hook) is a syntactic or
//! API-name convention of the language or framework, and is labelled as such where it is applied.

//! What the code graph knows about one file before cross-file resolution: the facts a language
//! plugin (`lang@1`, `<category>.extract`) extracted, plus what this plugin adds itself (content
//! hash, size, line count, text excerpt, the symbol-id prefix of the language and where the facts
//! came from).
//!
//! The fact types are `lang@1`'s own (`cc_protocol::contracts::lang`): extraction lives in the
//! language plugins, one per language, and nothing here parses source code.

use cc_protocol::contracts::lang;
use serde::{Deserialize, Serialize};

pub use lang::{
    CommandFact, DependencyFact, ImplFact, ImportFact, IpcCallFact, LocalFact, ParamFact, RefFact, RefKind, RefTarget, RequestFact, RunFact, SymbolFact,
    TEXT_EXCERPT_CHARS,
};

/// The language name of a text file no language plugin covers (Markdown, JSON, YAML, configs, plain
/// text, data): a file node with its leading text, searchable.
pub const TEXT: &str = "text";
/// The language name of a file that is not text (images, fonts, weights): a file node with its path,
/// type and size.
pub const BINARY: &str = "binary";

/// Where a file's facts came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// No language plugin covers the file: indexed as text or binary.
    #[default]
    Uncovered,
    /// Extracted by the language plugin `category` at `version`.
    Plugin { category: String, version: String },
    /// The plugin that covers the file failed on it (error, timeout, not UTF-8): indexed as text,
    /// with `error` as the file's note.
    Failed { category: String, error: String },
    /// Unchanged since the last index, which extracted it, but no plugin can extract it now (its
    /// plugin is not active yet, or failed): the graph keeps that index's nodes until one can.
    /// `plugin`: the one that failed (empty when none covers the file now).
    Kept { plugin: String, reason: String },
}

/// Everything extracted from one file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: String,
    pub language: String,
    pub content_hash: String,
    pub size: u64,
    pub lines: u32,
    pub symbols: Vec<SymbolFact>,
    pub refs: Vec<RefFact>,
    pub imports: Vec<ImportFact>,
    /// Names exported by the file (JS/TS `export`, Python `__all__`).
    pub exports: Vec<String>,
    pub requests: Vec<RequestFact>,
    pub ipc_calls: Vec<IpcCallFact>,
    pub runs: Vec<RunFact>,
    pub dependencies: Vec<DependencyFact>,
    pub commands: Vec<CommandFact>,
    /// Package name declared by a manifest, or by the file's package clause (Go `package x`,
    /// Java `package a.b;`).
    pub package: Option<String>,
    /// Rust `mod x;` declarations (file-backed modules) by name.
    pub mod_decls: Vec<String>,
    /// Rust `impl Trait for Type` blocks.
    pub impls: Vec<ImplFact>,
    /// Local variables whose type is evident from their declaration (receiver typing).
    pub locals: Vec<LocalFact>,
    /// Parse error count (tree-sitter ERROR nodes), informational.
    pub parse_errors: u32,
    /// The file's leading text (what its file node is searched by), at most
    /// [`TEXT_EXCERPT_CHARS`] characters; none for binary files.
    pub text: Option<String>,
    /// Prefix of the file's symbol ids (`<prefix>:<path>#<qname>`): the language's, from its
    /// plugin's `describe`.
    pub sid_prefix: String,
    pub origin: Origin,
}

impl FileFacts {
    /// The facts of a file's text before extraction: path, the caller's fields (content hash, size,
    /// line count, excerpt) and the text language.
    pub fn of_text(path: &str, text: &str) -> Self {
        FileFacts {
            path: path.to_owned(),
            language: TEXT.to_owned(),
            sid_prefix: "txt".to_owned(),
            content_hash: hash_hex(text.as_bytes()),
            size: text.len() as u64,
            lines: cc_protocol::hashline::file_lines(text).len() as u32,
            // Every file is findable by what it says, not only by its name or symbols (a script
            // with no functions, a README, a config).
            text: Some(excerpt(text)),
            ..FileFacts::default()
        }
    }

    /// Take what a language plugin extracted; the caller's fields (path, hash, size, lines,
    /// excerpt) stay this file's.
    pub fn absorb(&mut self, extracted: lang::FileFacts, sid_prefix: &str, origin: Origin) {
        self.language = extracted.language;
        self.symbols = extracted.symbols;
        self.refs = extracted.refs;
        self.imports = extracted.imports;
        self.exports = extracted.exports;
        self.requests = extracted.requests;
        self.ipc_calls = extracted.ipc_calls;
        self.runs = extracted.runs;
        self.dependencies = extracted.dependencies;
        self.commands = extracted.commands;
        self.package = extracted.package;
        self.mod_decls = extracted.mod_decls;
        self.impls = extracted.impls;
        self.locals = extracted.locals;
        self.parse_errors = extracted.parse_errors;
        self.sid_prefix = sid_prefix.to_owned();
        self.origin = origin;
    }

    /// Whether no language plugin's facts are behind this file (text or binary).
    pub fn is_plain(&self) -> bool {
        self.language == TEXT || self.language == BINARY
    }
}

/// The first [`TEXT_EXCERPT_CHARS`] characters of `text`.
pub fn excerpt(text: &str) -> String {
    text.chars().take(TEXT_EXCERPT_CHARS).collect()
}

/// Short stable content hash (BLAKE3, 128 bits as hex).
pub fn hash_hex(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    let mut out = String::with_capacity(32);
    for byte in &hash.as_bytes()[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
