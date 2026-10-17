//! package.json: the package name, `scripts` (with the repository files and sibling scripts each
//! one runs), `bin` entries, and dependencies by section (with the installed version and source
//! directory from `node_modules` when the repository is known).
//!
//! Ported from the memory plugin's manifest extractor (the package.json part of
//! plugins/memory-irongraph/backend/src/lang/manifest.rs), producing `lang@1` facts. What lives on
//! disk (installed packages, the files scripts and bins name) is resolved only when the extraction
//! context names the repository root.

use cc_lang_kit::ExtractContext;
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts};
use cc_lang_kit::manifest::{command_targets, dir_of, is_repo_file, join_rel, line_of};
use std::path::Path;

pub fn extract(path: &str, text: &str, ctx: &ExtractContext, facts: &mut FileFacts) {
    extract_with(path, text, ctx.root.as_deref(), facts);
}

/// [`extract`] with the repository root given directly.
pub fn extract_with(path: &str, text: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        facts.parse_errors = 1;
        return;
    };
    let dir = dir_of(path);
    facts.package = json.get("name").and_then(|n| n.as_str()).map(str::to_owned);
    if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
        for (name, command) in scripts {
            let Some(command) = command.as_str() else { continue };
            let (runs_files, runs_commands) = command_targets(command, dir, root);
            facts.commands.push(CommandFact {
                name: name.clone(),
                command: command.to_owned(),
                line: line_of(text, &format!("\"{name}\"")),
                runs_files,
                runs_commands,
                entry_point: None,
            });
        }
    }
    if let Some(bin) = json.get("bin") {
        let entries: Vec<(String, String)> = match bin {
            serde_json::Value::String(file) => vec![(facts.package.clone().unwrap_or_default(), file.clone())],
            serde_json::Value::Object(map) => map.iter().filter_map(|(k, v)| v.as_str().map(|f| (k.clone(), f.to_owned()))).collect(),
            _ => Vec::new(),
        };
        for (name, file) in entries {
            let rel = join_rel(dir, &file);
            let runs_files = if is_repo_file(root, &rel) { vec![rel] } else { Vec::new() };
            facts.commands.push(CommandFact {
                name: format!("bin:{name}"),
                command: file.clone(),
                line: line_of(text, "\"bin\""),
                runs_files,
                runs_commands: Vec::new(),
                entry_point: None,
            });
        }
    }
    for (section_key, section) in [("dependencies", "dependencies"), ("devDependencies", "dev"), ("peerDependencies", "peer"), ("optionalDependencies", "optional")] {
        let Some(deps) = json.get(section_key).and_then(|d| d.as_object()) else { continue };
        for (name, spec) in deps {
            let (installed_version, source_dir) = root.map(|root| npm_installed(root, dir, name)).unwrap_or((None, None));
            facts.dependencies.push(DependencyFact {
                name: name.clone(),
                ecosystem: "npm".into(),
                spec: spec.as_str().unwrap_or("").to_owned(),
                section: section.into(),
                installed_version,
                source_dir,
            });
        }
    }
}

/// Installed npm package: `node_modules/<name>` in the package directory or any ancestor up to the
/// repository root (Node's resolution order).
fn npm_installed(root: &Path, dir: &str, name: &str) -> (Option<String>, Option<String>) {
    let mut current = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    loop {
        let candidate = current.join("node_modules").join(name);
        let manifest = candidate.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            let version = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|j| j.get("version").and_then(|v| v.as_str()).map(str::to_owned));
            return (version, Some(candidate.to_string_lossy().into_owned()));
        }
        if current == root || !current.pop() || !current.starts_with(root) {
            return (None, None);
        }
    }
}
