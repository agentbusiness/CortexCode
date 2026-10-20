//! `lang@1`: one plugin per language (`lang-rust`, `lang-python`, `lang-go`, ...; the `lang`
//! family, see [`super::FAMILIES`]). A language plugin is everything CortexCode knows about one
//! language: its parser (the code graph's facts of a file), its language server (diagnostics,
//! definition, references, hover), its verifier commands and its skills. Methods are called on the
//! plugin's own category: `lang-go.extract`, `lang-go.diagnostics`, ... Consumers find the language
//! plugins with `frame.plugins.list` (contract `lang@1`) and pick one per file with `describe`.
//!
//! Positions are 1-based lines and columns (columns in characters); paths are relative to `root`.

use serde::{Deserialize, Serialize};

/// Method names (called as `<category>.<method>`).
pub mod methods {
    /// What the plugin covers: languages and their files, server, verifiers, skills.
    pub const DESCRIBE: &str = "describe";
    /// The code graph's facts of files ([`super::ExtractParams`] -> [`super::ExtractResult`]).
    pub const EXTRACT: &str = "extract";
    /// The language server's diagnostics for files of a workspace (after their latest text).
    pub const DIAGNOSTICS: &str = "diagnostics";
    /// Where the symbol at a position is defined.
    pub const DEFINITION: &str = "definition";
    /// Where the symbol at a position is used.
    pub const REFERENCES: &str = "references";
    /// The language server's description of the symbol at a position (type, signature, docs).
    pub const HOVER: &str = "hover";
    /// One skill's text.
    pub const SKILL: &str = "skill";
    /// Install the language server (and tools) into CortexCode's toolchains directory.
    pub const INSTALL: &str = "install";
    /// Every method of the contract (the plugin's `provides`, prefixed by its category).
    pub const ALL: &[&str] = &[DESCRIBE, EXTRACT, DIAGNOSTICS, DEFINITION, REFERENCES, HOVER, SKILL, INSTALL];
}

/// Topics published as `<category>.<topic>`.
pub mod topics {
    /// The language server's state changed (installed, started, stopped, failed).
    pub const SERVER: &str = "server";
}

/// The contract name.
pub const CONTRACT: &str = "lang@1";

/// Which declared verifiers apply to a project, and the check commands they offer.
pub mod verifiers;

// ----- describe -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Description {
    /// The plugin's language family name (`rust`, `python`, `typescript`, `go`, `java`, `cpp`, `shell`).
    pub id: String,
    pub name: String,
    /// The languages it parses (one plugin may cover several: typescript, tsx, javascript).
    pub languages: Vec<LanguageInfo>,
    pub server: Option<ServerInfo>,
    pub verifiers: Vec<Verifier>,
    pub skills: Vec<SkillInfo>,
    /// Directories of the toolchain this plugin installed (Go, a JDK, ...) that commands need on
    /// PATH (the tools plugin prepends them to every command's PATH). Empty when the system's own
    /// tools serve.
    #[serde(default)]
    pub path: Vec<String>,
    /// Environment the toolchain needs in commands (`JAVA_HOME`, `GOROOT`, ...).
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LanguageInfo {
    /// The name facts carry (`FileFacts.language`).
    pub name: String,
    /// Prefix of symbol ids in the code graph (`rs`, `py`, `go`, ...).
    pub sid_prefix: String,
    /// File extensions without the dot.
    pub extensions: Vec<String>,
    /// Exact file names (`Cargo.toml`, `go.mod`, `pom.xml`).
    pub filenames: Vec<String>,
    /// Interpreters named by a `#!` line (`bash`, `python3`).
    pub shebangs: Vec<String>,
    /// File-name patterns (`Dockerfile.*`, `docker-compose.*.yml`, `*.Dockerfile`): `*` matches any
    /// run of characters (none included), everything else matches itself (case-sensitive); the
    /// whole file name must match ([`name_matches`]).
    #[serde(default)]
    pub patterns: Vec<String>,
}

impl LanguageInfo {
    /// Whether a file name (no directory) is one of [`Self::filenames`] or matches one of
    /// [`Self::patterns`].
    pub fn matches_name(&self, name: &str) -> bool {
        self.filenames.iter().any(|f| f == name) || self.patterns.iter().any(|p| name_matches(p, name))
    }

    /// Whether this language takes a file by its name (no directory): a file name, a pattern or
    /// an extension (case-insensitive) it declares.
    pub fn covers(&self, name: &str) -> bool {
        self.matches_name(name)
            || name.rsplit_once('.').is_some_and(|(stem, extension)| !stem.is_empty() && self.extensions.iter().any(|e| e.eq_ignore_ascii_case(extension)))
    }
}

/// Whether the file name `name` matches `pattern`, where `*` stands for any run of characters
/// (none included) and every other character for itself.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else { return false };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else { return rest.is_empty() };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerInfo {
    /// `rust-analyzer`, `gopls`, ...
    pub name: String,
    /// Where it runs from when installed.
    pub path: Option<String>,
    pub installed: bool,
    pub version: Option<String>,
    /// Why it is not available, in words, when it is not.
    pub problem: Option<String>,
}

/// A command that checks work in this language, run in a project that has `when` (any of the files).
///
/// Placeholders in `command`: an argument that is exactly `{file}` is replaced by the one file
/// being checked, an argument that is exactly `{files}` by every file being checked, one argument
/// each (paths relative to the project root, where the command runs). A verifier without
/// placeholders checks the whole project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Verifier {
    pub name: String,
    /// What the command checks, as the model reads it: `build` (compiles), `typecheck` (types, no
    /// output), `syntax` (parses only), `test`, `lint` or `format`.
    pub kind: String,
    pub command: Vec<String>,
    /// Marker files: the verifier applies to a project with one of them at its root (none: it
    /// applies to any project).
    pub when: Vec<String>,
    /// Programs the command needs on PATH (`ruff`, `docker`): the consumer checks them, on the PATH
    /// commands run with (the language plugins' toolchain `path` included), and leaves the verifier
    /// out when one is missing.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Files the command needs to have anything to check, as patterns relative to the project root
    /// (`**/*.test.*`; `*` within a name, `**` any directories): the consumer leaves the verifier
    /// out when no file of the project matches one (a test runner with no tests can only fail).
    /// None: no such need.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    /// One line: when the skill helps.
    pub summary: String,
    /// Diagnostic codes the skill covers, exactly as the tools emit them (a language server's
    /// diagnostic `code`, a compiler's or linter's code in its output: `E0382`, `reportMissingImports`,
    /// `SC2086`, `DL3008`). Declared by the plugin that owns the skill (`data/skills/codes.toml`);
    /// consumers attach the skill when one of these codes shows up. Not meant for the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<String>,
    /// The projects the skill can help in, declared by the plugin that owns the skill
    /// (`data/skills/applies.toml`); empty: every project. Consumers offer the skill only to a
    /// project that matches one of the conditions ([`Applies::matches`]).
    #[serde(default, skip_serializing_if = "Applies::is_unset")]
    pub applies: Applies,
}

/// Structural facts a project must show (any one) for a skill to apply to it. Every list is
/// optional; all empty means the skill applies everywhere.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Applies {
    /// Languages as the code graph names a project's languages (`FileFacts.language`: `html`,
    /// `css`, `tsx`, `c`, `compose`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// Repository-relative file paths, one of which must exist ([`path_matches`]: `*.sql`,
    /// `Dockerfile`, `migrations/**/*.sql`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Packages the project's code imports or its manifests declare, as the code graph records
    /// them (`pandas`, `react`, `database/sql`, `org.neo4j`); [`package_matches`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Always loaded where it applies, never a choice: a language plugin's guide to working in
    /// its language goes into the session's system message for every project in that language.
    #[serde(default, skip_serializing_if = "is_false")]
    pub always: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Applies {
    /// No condition declared (the skill applies everywhere); `always` is how it loads, not a
    /// condition.
    pub fn is_empty(&self) -> bool {
        self.languages.is_empty() && self.files.is_empty() && self.imports.is_empty()
    }

    /// Nothing declared at all (no condition and not `always`): left out of a description.
    pub fn is_unset(&self) -> bool {
        self.is_empty() && !self.always
    }

    /// Whether a project with these languages, repository files and packages shows one of the
    /// conditions (always, when none is declared).
    pub fn matches(&self, languages: &[String], files: &[String], packages: &[String]) -> bool {
        self.is_empty()
            || self.languages.iter().any(|language| languages.contains(language))
            || files.iter().any(|path| self.files.iter().any(|glob| path_matches(glob, path)))
            || packages.iter().any(|package| self.imports.iter().any(|wanted| package_matches(wanted, package)))
    }
}

/// Whether the repository-relative `path` matches `glob`. Segments are split at `/`; `**` stands
/// for any number of directories (none included), and inside a segment `*` for any run of
/// characters ([`name_matches`]). A glob without `/` matches the file name in any directory
/// (`*.sql` is `**/*.sql`).
pub fn path_matches(glob: &str, path: &str) -> bool {
    fn segments(glob: &[&str], path: &[&str]) -> bool {
        match glob.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| segments(rest, &path[skip..])),
            Some((first, rest)) => path.split_first().is_some_and(|(name, tail)| name_matches(first, name) && segments(rest, tail)),
        }
    }
    let glob: Vec<&str> = if glob.contains('/') { glob.split('/').collect() } else { vec!["**", glob] };
    let path: Vec<&str> = path.split('/').collect();
    segments(&glob, &path)
}

/// Whether the recorded package `package` is `wanted` or inside it: equal, or `wanted` followed
/// by a separator (`/`, `.`, `::`), so `org.neo4j` matches `org.neo4j.driver` and
/// `github.com/jackc/pgx` matches `github.com/jackc/pgx/v5`, while `react` does not match `react-dom`.
pub fn package_matches(wanted: &str, package: &str) -> bool {
    package.strip_prefix(wanted).is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('.') || rest.starts_with("::"))
}

// ----- extract -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SourceFile {
    /// Repository-relative path.
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtractParams {
    pub files: Vec<SourceFile>,
    /// The repository root on disk (absolute), for facts that need more than the file: lockfiles,
    /// installed dependency sources, which files a manifest's targets name.
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractResult {
    /// One per file, in order. Content hash, size, line count and text excerpt are the caller's.
    pub files: Vec<FileFacts>,
}

// ----- language server -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticsParams {
    /// The workspace root the server works on.
    pub root: String,
    /// Files whose diagnostics are wanted (relative to `root`); empty = every file the server reported.
    #[serde(default)]
    pub paths: Vec<String>,
    /// How long to wait for the server to settle after it saw the latest text (default plugin's).
    /// 0: what the server has now, without waiting for anything: a server not running for `root`
    /// (or busy starting) answers `partial` with nothing (one is started in the background for
    /// named files), a running one is not re-checked.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// `error` | `warning` | `info` | `hint`
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    /// The tool that reported it (`rust-analyzer`, `rustc`, `clippy`, `pyright`, ...).
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticsResult {
    pub server: String,
    pub diagnostics: Vec<Diagnostic>,
    /// The server had not finished analysing when the wait ended (diagnostics may be incomplete).
    #[serde(default)]
    pub partial: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PositionParams {
    pub root: String,
    pub path: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// The text of the line at `line`, for showing without a read.
    #[serde(default)]
    pub line_text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LocationsResult {
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HoverResult {
    /// Markdown or plain text; empty when the server has nothing for the position.
    pub text: String,
}

// ----- skills, install -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SkillParams {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SkillResult {
    pub name: String,
    /// Markdown.
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstallResult {
    pub server: ServerInfo,
    /// What the installation did, in order.
    pub log: Vec<String>,
}

// ----- the code graph's facts of a file (produced by `extract`) -----

/// One parameter of a function, method or component.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParamFact {
    pub name: String,
    pub position: u32,
    pub type_text: Option<String>,
    pub default_value: Option<String>,
    pub optional: bool,
    pub variadic: bool,
    /// Type names mentioned in the annotation (for OF_TYPE edges).
    pub type_names: Vec<String>,
}

/// A definition in a file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolFact {
    /// function | method | class | struct | trait | interface | enum | type | const | static |
    /// field | variant | variable | component | prop | module | macro | test | route |
    /// command | shell_function | ipc_channel
    pub kind: String,
    pub name: String,
    /// Qualified name inside the file (`Type::method`, `Class.method`, `mod::f`).
    pub qname: String,
    pub signature: String,
    pub doc: Option<String>,
    pub code: String,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    /// Hash of the body text only (rename detection); empty when there is no body.
    pub body_hash: String,
    /// Hash of the signature text.
    pub sig_hash: String,
    /// Index of the containing symbol in [`FileFacts::symbols`].
    pub parent: Option<usize>,
    pub params: Vec<ParamFact>,
    pub return_type: Option<String>,
    pub return_type_names: Vec<String>,
    /// Field / const / variable type text and literal or default value.
    pub type_text: Option<String>,
    pub value: Option<String>,
    pub type_names: Vec<String>,
    pub is_async: bool,
    pub is_static: bool,
    pub exported: bool,
    pub visibility: Option<String>,
    pub extends: Vec<String>,
    pub implements: Vec<String>,
    pub decorators: Vec<String>,
    /// For trait-impl methods (`impl Trait for Type`): the trait name (OVERRIDES).
    pub impl_trait: Option<String>,
    /// For routes: HTTP method and path; for IPC channels: the channel name in `path`.
    pub http_method: Option<String>,
    pub route_path: Option<String>,
    /// Route / channel handler: the referenced function name and qualifier.
    pub handler: Option<RefTarget>,
    /// For tests: the framework (`rust`, `pytest`, `js`, `go`, `junit`, `testng`, `gtest`, `catch2`...).
    pub framework: Option<String>,
    /// What its own body says in words, in order: comments and message strings (not those of
    /// definitions nested in it, not its doc), [`NOTES_PER_SYMBOL`] at most.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A name as written at a reference site: `name`, optionally qualified (`a::b`, `obj.`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RefTarget {
    pub name: String,
    /// Qualifier text before the name (`Type` in `Type::new`, `self`, `this`, `api` in `api.x`).
    /// C/C++ scope qualifiers keep their trailing `::` (`std::` in `std::sort`, `a::B::` in
    /// `a::B::f`), which tells them apart from object receivers (`v` in `v.size()`, `p->f()`).
    pub qualifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefKind {
    Call,
    Instantiate,
    Render,
    UseHook,
    Read,
    Write,
    Throw,
    ReadConfig,
    /// Prose naming a symbol (a backtick identifier in a document section): linked when exactly
    /// one symbol of the project has that name or qualified name (`MENTIONS`).
    Mention,
}

/// A reference from a definition (or the file's top level) to a name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefFact {
    /// Index of the enclosing symbol; `None` for top-level code.
    pub from: Option<usize>,
    pub kind: RefKind,
    pub target: RefTarget,
    /// Argument expressions (calls) or attribute texts (renders), as written.
    pub args: Vec<String>,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportFact {
    /// Module specifier as written (`./api`, `react`, `crate::db`, `.db`, `os.path`).
    pub module: String,
    /// Imported name in the source module (`*` for namespace / glob, `default` for default).
    pub name: String,
    /// Local binding name.
    pub local: String,
    pub line: u32,
}

/// An outgoing HTTP request with a literal URL (fetch / axios): a cross-language edge source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFact {
    pub from: Option<usize>,
    pub method: String,
    /// URL path with template substitutions replaced by `{}`.
    pub url: String,
    pub line: u32,
    pub args: Vec<String>,
}

/// An IPC invocation (Electron ipcRenderer, Tauri invoke) by channel name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcCallFact {
    pub from: Option<usize>,
    pub channel: String,
    pub line: u32,
    pub args: Vec<String>,
}

/// A literal that names a script or program run by this code (shell command, subprocess).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFact {
    pub from: Option<usize>,
    /// Path as written (relative to the file's directory or the repo root).
    pub target: String,
    pub line: u32,
}

/// A dependency declared by a manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DependencyFact {
    pub name: String,
    /// npm | cargo | pypi
    pub ecosystem: String,
    pub spec: String,
    /// dependencies | dev | build | peer | optional
    pub section: String,
    pub installed_version: Option<String>,
    /// Absolute directory of the installed source, when present locally.
    pub source_dir: Option<String>,
}

/// A named script or command declared by a manifest (npm scripts, cargo aliases, bins, entry points).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandFact {
    pub name: String,
    /// What it runs, as written.
    pub command: String,
    pub line: u32,
    /// Repository-relative files the command runs (resolved structurally from its words).
    pub runs_files: Vec<String>,
    /// Other commands of the same manifest it invokes (`npm run x`).
    pub runs_commands: Vec<String>,
    /// `module:function` entry point (pyproject scripts).
    pub entry_point: Option<String>,
}

/// `impl Trait for Type` (Rust): Type IMPLEMENTS Trait.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImplFact {
    pub type_name: String,
    pub trait_name: String,
    pub line: u32,
}

/// A local variable of a definition with the type named by its annotation or initializer
/// (`let s = Store::new()`, `s = Store()`, `const s = new Store()`, `s: Store = ...`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalFact {
    pub from: usize,
    pub name: String,
    pub type_name: String,
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
    /// Comments written on their own lines directly above a call: what the caller says the call
    /// does, by the call's line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub line_notes: Vec<LineNote>,
}

/// A comment on its own line(s) directly above `line` (1-based), as words.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LineNote {
    pub line: u32,
    pub text: String,
}

/// Body notes kept per definition ([`SymbolFact::notes`]) and characters kept per note.
pub const NOTES_PER_SYMBOL: usize = 6;
pub const NOTE_CHARS: usize = 160;

/// Leading characters of a text file kept on its node for search: IronGraph's embedding reads a
/// bounded input, and a file's beginning (title, header, first keys) is what says what it is.
pub const TEXT_EXCERPT_CHARS: usize = 2000;
