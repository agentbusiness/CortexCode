//! The language servers: vscode-html-language-server and vscode-css-language-server, both from
//! the npm package vscode-langservers-extracted (VS Code's own HTML and CSS servers, extracted),
//! run with Node.js over stdio. `lang@1`'s kit drives one server process per workspace, so that
//! process is this plugin's own executable in router mode (`cc-lang-web lsp-router`,
//! [`crate::router`]), which starts both and routes each document to its server by extension.
//!
//! The package is installed with `npm install --prefix` into
//! `$CORTEX_HOME/toolchains/vscode-langservers-extracted/<version>/`; nothing global is touched.

use cc_lang_kit::install::{FoundServer, PROBE_TIMEOUT, Toolchains, find_on_path, is_executable, npm_installed_version, run};
use cc_lang_kit::lsp::ServerSpec;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server's name in descriptions and diagnostics: the two servers behind the router.
pub const NAME: &str = "vscode-html-language-server+vscode-css-language-server";

pub const TOOLCHAIN: &str = "vscode-langservers-extracted";
pub const PACKAGE: &str = "vscode-langservers-extracted";
pub const VERSION: &str = "4.10.0";
/// The bin scripts of the two servers, relative to the package directory.
pub const HTML_SCRIPT: &str = "bin/vscode-html-language-server";
pub const CSS_SCRIPT: &str = "bin/vscode-css-language-server";
/// The oldest Node.js major the servers (vscode-languageserver 10) run on.
pub const NODE_MAJOR: u32 = 18;

fn node_candidates() -> Vec<PathBuf> {
    let mut out = find_on_path("node");
    for known in ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/opt/homebrew/opt/node@22/bin/node", "/opt/homebrew/opt/node@24/bin/node", "/opt/homebrew/opt/node@20/bin/node"] {
        let path = PathBuf::from(known);
        if !out.contains(&path) && is_executable(&path) {
            out.push(path);
        }
    }
    out
}

/// `v22.23.2` -> 22.
pub fn node_major(version: &str) -> Option<u32> {
    version.trim().trim_start_matches('v').split('.').next()?.parse().ok()
}

/// A Node.js at least `major`, with its version.
pub fn find_node(major: u32) -> Result<(PathBuf, String), String> {
    let mut seen = Vec::new();
    for node in node_candidates() {
        match run(&node, &["--version".into()], None, &[], PROBE_TIMEOUT) {
            Ok(ran) if ran.success => {
                let version = ran.stdout.trim().to_owned();
                if node_major(&version).is_some_and(|m| m >= major) {
                    return Ok((node, version));
                }
                seen.push(format!("{} is {version}", node.display()));
            }
            Ok(ran) => seen.push(format!("{} --version failed ({})", node.display(), ran.status)),
            Err(error) => seen.push(error),
        }
    }
    if seen.is_empty() {
        Err(format!("Node.js {major} or newer is needed and none was found (PATH, Homebrew)"))
    } else {
        Err(format!("Node.js {major} or newer is needed: {}", seen.join("; ")))
    }
}

/// The installed package directory, when this exact version is installed.
pub fn package_dir(toolchains: &Toolchains) -> Result<PathBuf, String> {
    let prefix = toolchains.version_dir(TOOLCHAIN, VERSION)?;
    if npm_installed_version(&prefix, PACKAGE).as_deref() != Some(VERSION) {
        return Err(format!("{PACKAGE}@{VERSION} is not installed in {}", prefix.display()));
    }
    Ok(prefix.join("node_modules").join(PACKAGE))
}

/// What the router runs: Node.js and the two servers' scripts.
#[derive(Debug, Clone, PartialEq)]
pub struct Children {
    pub node: PathBuf,
    pub node_version: String,
    pub html: PathBuf,
    pub css: PathBuf,
}

impl Children {
    pub fn version(&self) -> String {
        format!("{PACKAGE} {VERSION} (node {})", self.node_version)
    }
}

/// The installed servers and a Node.js that runs them.
pub fn children(toolchains: &Toolchains) -> Result<Children, String> {
    let mut problems = Vec::new();
    let scripts = package_dir(toolchains).and_then(|dir| {
        let (html, css) = (dir.join(HTML_SCRIPT), dir.join(CSS_SCRIPT));
        match (html.is_file(), css.is_file()) {
            (true, true) => Ok((html, css)),
            (false, _) => Err(format!("{} is missing", html.display())),
            (_, false) => Err(format!("{} is missing", css.display())),
        }
    });
    let scripts = scripts.map_err(|e| problems.push(e)).ok();
    let node = find_node(NODE_MAJOR).map_err(|e| problems.push(e)).ok();
    match (node, scripts) {
        (Some((node, node_version)), Some((html, css))) => Ok(Children { node, node_version, html, css }),
        _ => Err(problems.join("; ")),
    }
}

/// Whether a document goes to the CSS server (`.css`, `.scss`, `.less`); everything else is the
/// HTML server's.
pub fn is_stylesheet(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    [".css", ".scss", ".less"].iter().any(|ext| name.ends_with(ext))
}

/// `initializationOptions` of the HTML server: embedded CSS and JavaScript on, formatting off.
pub fn html_init_options() -> Value {
    json!({"provideFormatter": false, "embeddedLanguages": {"css": true, "javascript": true}, "handledSchemas": ["file"]})
}

/// `initializationOptions` of the CSS server.
pub fn css_init_options() -> Value {
    json!({"provideFormatter": false, "handledSchemas": ["file"]})
}

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = vec!["lsp-router".into()];
    spec.version_args = vec!["lsp-router".into(), "--version".into()];
    spec.language_ids = [("html", "html"), ("htm", "html"), ("css", "css"), ("scss", "scss"), ("less", "less")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    // `workspace/configuration` by section: the HTML server asks `html`, `css`, `javascript`; the
    // CSS server asks the document's language. Validation on (the servers' defaults), embedded
    // scripts and styles validated in pages.
    spec.settings = json!({
        "html": {"validate": {"scripts": true, "styles": true}, "hover": {"documentation": true, "references": false}},
        "css": {"validate": true, "hover": {"documentation": true, "references": false}},
        "scss": {"validate": true, "hover": {"documentation": true, "references": false}},
        "less": {"validate": true, "hover": {"documentation": true, "references": false}},
        "javascript": {"validate": {"enable": true}}
    });
    // Both servers validate in-process, half a second after a change.
    spec.diagnostics_wait = Duration::from_secs(20);
    spec.query_wait = Duration::from_secs(15);
    spec.init_timeout = Duration::from_secs(30);
    spec
}

/// The router (this executable) when both servers can run.
pub fn locate(toolchains: &Toolchains) -> Result<FoundServer, String> {
    let children = children(toolchains)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this plugin's executable: {e}"))?;
    Ok(FoundServer { path: exe, version: children.version() })
}

/// Install the package (npm, into the toolchains directory).
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    let (_, node_version) = find_node(NODE_MAJOR)?;
    log.push(format!("node {node_version}"));
    toolchains.npm_install(TOOLCHAIN, PACKAGE, VERSION, log)?;
    Ok(())
}

/// A project's own executable in `node_modules/.bin` (the verifiers run it with `npx --no`).
pub fn project_bin(root: &Path, name: &str) -> PathBuf {
    root.join("node_modules").join(".bin").join(name)
}
