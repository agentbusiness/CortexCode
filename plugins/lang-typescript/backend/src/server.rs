//! typescript-language-server: the LSP front of TypeScript's `tsserver`, for TypeScript and
//! JavaScript alike. Installed by `lang-typescript.install` into CortexCode's toolchains directory
//! (`npm install --prefix`, never globally): `toolchains/typescript-language-server/<version>/` and
//! the TypeScript it falls back to, `toolchains/typescript/<version>/`. Otherwise one on PATH is
//! used. Either is accepted only when `typescript-language-server --version` runs (it is a Node.js
//! script: `node` must be on PATH).
//!
//! Which TypeScript serves a workspace is the server's own choice, in its order: the workspace's
//! `node_modules/typescript` (also Yarn / pnpm SDKs) when it provides `tsserver.js`, then the
//! `tsserver.fallbackPath` initialization option (the toolchain TypeScript), then the TypeScript
//! next to the server. `tsserver.path` is deliberately not set: it would override the workspace's
//! own TypeScript. [`typescript_for`] mirrors that order so the server's description says which
//! TypeScript serves a root.
//!
//! TypeScript 7 (the native compiler, `typescript@7` on npm) ships no `tsserver.js`, so a workspace
//! on it is served by the fallback TypeScript 6 (the server warns about it). TypeScript 7 has a
//! language server of its own (`tsc --lsp --stdio`), not used here. The kit no longer stands in
//! the way: a pack's `server_launch` can start another program per root (the workspace's
//! `node_modules/typescript/bin/tsc --lsp --stdio` for a root whose TypeScript has no
//! tsserver.js), servers that advertise `diagnosticProvider` are asked for diagnostics with
//! `textDocument/diagnostic`, and lower-cased URIs are spelled as the workspace spells them on
//! macOS. It is not added because it could not be checked against the real server here (no
//! TypeScript 7 is installed on this machine and installs were out of bounds for the change), and
//! what the kit relies on is unconfirmed for it: that it advertises `diagnosticProvider` to a
//! client that does not declare pull support (the kit declares none, so rust-analyzer keeps
//! pushing), that it answers definition / references / hover for `.js` outside a project as this
//! pack promises, and how it reports a project still loading (the kit waits on work-done progress).
//! A server that only answers pulls and does not advertise them would leave every diagnostics call
//! partial after the full wait, which is worse than the TypeScript 6 fallback that serves now.
//!
//! JavaScript-only projects: files outside any tsconfig.json / jsconfig.json form the server's
//! inferred project, whose defaults (`implicitProjectConfiguration`) keep `checkJs` off, so plain
//! JavaScript gets navigation and syntax errors, not type errors; a jsconfig.json with `checkJs`
//! turns them on. Automatic type acquisition (an npm download of `@types` packages) is disabled:
//! CortexCode works offline.

use cc_lang_kit::install::{FoundServer, Toolchains, find_existing, find_on_path};
use cc_lang_kit::lsp::ServerSpec;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "typescript-language-server";
/// The pinned typescript-language-server (npm), installed into `toolchains/typescript-language-server/<version>`.
pub const SERVER_VERSION: &str = "6.0.2";
/// The pinned TypeScript the server falls back to, installed into `toolchains/typescript/<version>`.
/// TypeScript 6 is the last line that ships `tsserver.js` (TypeScript 7 is the native compiler).
pub const TYPESCRIPT: &str = "typescript";
pub const TYPESCRIPT_VERSION: &str = "6.0.3";

/// `languageId` of documents by extension (the ids tsserver's LSP front expects).
const LANGUAGE_IDS: &[(&str, &str)] = &[
    ("ts", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("tsx", "typescriptreact"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascriptreact"),
];

/// The toolchain server executable (`node_modules/.bin/typescript-language-server`).
pub fn toolchain_server(toolchains: &Toolchains) -> Option<PathBuf> {
    toolchains.version_dir(NAME, SERVER_VERSION).ok().map(|dir| dir.join("node_modules").join(".bin").join(NAME))
}

/// The toolchain TypeScript's `tsserver.js`.
pub fn toolchain_tsserver(toolchains: &Toolchains) -> Option<PathBuf> {
    toolchains.version_dir(TYPESCRIPT, TYPESCRIPT_VERSION).ok().map(|dir| dir.join("node_modules").join(TYPESCRIPT).join("lib").join("tsserver.js"))
}

pub fn spec(home: &Path) -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = vec!["--stdio".into()];
    spec.language_ids = LANGUAGE_IDS.iter().map(|(ext, id)| ((*ext).to_owned(), (*id).to_owned())).collect();
    let mut options = json!({
        "hostInfo": "CortexCode",
        // Offline: no npm downloads of @types packages by tsserver's typings installer.
        "disableAutomaticTypingAcquisition": true,
    });
    if let Some(tsserver) = toolchain_tsserver(&Toolchains::new(home)) {
        // The fixed path is passed whether or not it is installed yet: the spec is built once, and
        // the server skips an invalid fallback (it then uses the TypeScript next to itself).
        options["tsserver"] = json!({"fallbackPath": tsserver.to_string_lossy()});
    }
    spec.initialization_options = options;
    // tsserver publishes a file's syntactic diagnostics first and its semantic ones after the type
    // check of the program, without progress in between: the quiet period that counts as settled
    // covers a first check of a larger program.
    spec.settle = Duration::from_secs(2);
    // Loading a project (tsconfig, every source and declaration file) is reported as progress.
    spec.diagnostics_wait = Duration::from_secs(90);
    spec.query_wait = Duration::from_secs(60);
    spec
}

/// Module folders the server looks for TypeScript in, at the workspace root and each ancestor
/// (typescript-language-server's `MODULE_FOLDERS`; the nearest directory with any of them wins).
const MODULE_FOLDERS: &[&str] = &["node_modules/typescript/lib", ".vscode/pnpify/typescript/lib", ".yarn/sdks/typescript/lib", ".pnpm/sdks/typescript/lib"];

/// Which TypeScript the server runs for a workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeScriptUse {
    /// The workspace's own (`lib` folder, version).
    Workspace { lib: PathBuf, version: Option<String> },
    /// The toolchain TypeScript (`tsserver.fallbackPath`); `unusable` names the workspace's
    /// TypeScript that has no tsserver.js (TypeScript 7) when there is one.
    Fallback { version: String, unusable: Option<(PathBuf, Option<String>)> },
    /// Neither: the server uses the TypeScript beside itself, if it has one.
    Beside { unusable: Option<(PathBuf, Option<String>)> },
}

fn package_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok()?.get("version")?.as_str().map(str::to_owned)
}

/// The server's choice of TypeScript for `root`, in its own order: the nearest workspace
/// TypeScript (from `root` up) when it ships tsserver.js (TypeScript <= 6), else the toolchain
/// TypeScript, else the one beside the server.
pub fn typescript_for(root: &Path, toolchains: &Toolchains) -> TypeScriptUse {
    let mut unusable = None;
    let found = root.ancestors().find_map(|dir| MODULE_FOLDERS.iter().map(|m| dir.join(m)).find(|p| p.exists()));
    if let Some(lib) = found {
        let version = lib.parent().and_then(package_version);
        if lib.join("tsserver.js").is_file() {
            return TypeScriptUse::Workspace { lib, version };
        }
        unusable = Some((lib, version));
    }
    match toolchain_tsserver(toolchains).filter(|p| p.is_file()) {
        Some(_) => TypeScriptUse::Fallback { version: TYPESCRIPT_VERSION.into(), unusable },
        None => TypeScriptUse::Beside { unusable },
    }
}

impl TypeScriptUse {
    /// In words, for the server's description.
    pub fn describe(&self) -> String {
        let unusable_words = |unusable: &Option<(PathBuf, Option<String>)>| match unusable {
            Some((lib, version)) => format!(
                "; the workspace's TypeScript {}at {} has no tsserver.js",
                version.as_deref().map(|v| format!("{v} ")).unwrap_or_default(),
                lib.display()
            ),
            None => String::new(),
        };
        match self {
            TypeScriptUse::Workspace { lib, version } => format!("workspace TypeScript {} at {}", version.as_deref().unwrap_or("(unknown version)"), lib.display()),
            TypeScriptUse::Fallback { version, unusable } => format!("CortexCode's TypeScript {version}{}", unusable_words(unusable)),
            TypeScriptUse::Beside { unusable } => format!("the TypeScript beside the server{}", unusable_words(unusable)),
        }
    }
}

/// A runnable typescript-language-server: the toolchain one (with its fallback TypeScript), else
/// one on PATH. With a workspace `root`, the version also says which TypeScript will serve it.
pub fn locate(toolchains: &Toolchains, root: Option<&Path>) -> Result<FoundServer, String> {
    let mut problems = Vec::new();
    let mut candidates = Vec::new();
    if let Some(server) = toolchain_server(toolchains).filter(|p| p.exists()) {
        if toolchain_tsserver(toolchains).is_some_and(|p| p.is_file()) {
            candidates.push(server);
        } else {
            problems.push(format!("{} has no TypeScript {TYPESCRIPT_VERSION} beside it (`lang-typescript.install` installs both)", server.display()));
        }
    }
    candidates.extend(find_on_path(NAME));
    match find_existing(&candidates, &["--version".into()], root, &[]) {
        Ok(mut found) => {
            if let Some(root) = root {
                found.version = format!("{}; {}", found.version, typescript_for(root, toolchains).describe());
            }
            Ok(found)
        }
        Err(problem) => {
            if problem != "no executable found" {
                problems.push(problem);
            }
            if find_on_path("node").is_empty() {
                problems.push("Node.js (`node`) is not on PATH; typescript-language-server runs on it".into());
            }
            if problems.is_empty() {
                Err(format!("{NAME} is not installed (not in CortexCode's toolchains or on PATH)"))
            } else {
                Err(format!("{NAME} is not installed: {}", problems.join("; ")))
            }
        }
    }
}

/// `npm install --prefix` of the pinned typescript-language-server and TypeScript into the
/// toolchains directory (each kept when that version is already complete).
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    toolchains.npm_install(NAME, NAME, SERVER_VERSION, log)?;
    toolchains.npm_install(TYPESCRIPT, TYPESCRIPT, TYPESCRIPT_VERSION, log)?;
    Ok(())
}
