//! pyproject.toml: the project name (PEP 621 `[project]` or Poetry's `[tool.poetry]`), dependencies
//! (PEP 508 requirements of `[project] dependencies` and `optional-dependencies`, Poetry's
//! `dependencies` / `dev-dependencies` tables) with the installed version and source directory when
//! a virtual environment of the project holds them, and the scripts (`[project.scripts]` /
//! `[tool.poetry.scripts]`) as commands with their `module:function` entry points.
//!
//! Ported from the memory plugin's manifest extractor (the pyproject part of
//! plugins/memory-irongraph/backend/src/lang/manifest.rs), producing `lang@1` facts. Installed
//! versions are read only when the extraction context names the repository root.

use cc_lang_kit::ExtractContext;
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts};
use cc_lang_kit::manifest::{dir_of, line_of};
use std::path::{Path, PathBuf};

pub fn extract(path: &str, text: &str, ctx: &ExtractContext, facts: &mut FileFacts) {
    extract_with(path, text, ctx.root.as_deref(), facts);
}

/// [`extract`] with an explicit repository root.
pub fn extract_with(path: &str, text: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Ok(doc) = text.parse::<toml::Table>() else {
        facts.parse_errors = 1;
        return;
    };
    let dir = dir_of(path);
    let project = doc.get("project").and_then(|p| p.as_table());
    let poetry = doc.get("tool").and_then(|t| t.get("poetry")).and_then(|p| p.as_table());
    facts.package = project.and_then(|p| p.get("name")).or_else(|| poetry.and_then(|p| p.get("name"))).and_then(|n| n.as_str()).map(str::to_owned);
    let site = root.and_then(|root| site_packages(root, dir));
    let push_dep = |name: String, spec: String, section: &str, facts: &mut FileFacts| {
        if name.is_empty() || name == "python" {
            return;
        }
        let (installed_version, source_dir) = site.as_ref().map(|s| pypi_installed(s, &name)).unwrap_or((None, None));
        facts.dependencies.push(DependencyFact { name, ecosystem: "pypi".into(), spec, section: section.into(), installed_version, source_dir });
    };
    let add_dep = |requirement: &str, section: &str, facts: &mut FileFacts| {
        // PEP 508: the distribution name is the leading identifier.
        let name: String = requirement.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
        let spec = requirement.trim()[name.len()..].trim().to_owned();
        push_dep(name, spec, section, facts);
    };
    if let Some(project) = project {
        for dep in project.get("dependencies").and_then(|d| d.as_array()).into_iter().flatten() {
            if let Some(r) = dep.as_str() {
                add_dep(r, "dependencies", facts);
            }
        }
        if let Some(optional) = project.get("optional-dependencies").and_then(|o| o.as_table()) {
            for deps in optional.values().filter_map(|v| v.as_array()) {
                for dep in deps {
                    if let Some(r) = dep.as_str() {
                        add_dep(r, "optional", facts);
                    }
                }
            }
        }
    }
    if let Some(poetry) = poetry {
        for (key, section) in [("dependencies", "dependencies"), ("dev-dependencies", "dev")] {
            if let Some(deps) = poetry.get(key).and_then(|d| d.as_table()) {
                // Poetry keys are the names and values the constraints (`"24.1"` is a version, not
                // part of the name), as a string or a table's `version`.
                for (name, value) in deps {
                    let spec = value.as_str().or_else(|| value.get("version").and_then(|v| v.as_str())).unwrap_or_default().to_owned();
                    push_dep(name.clone(), spec, section, facts);
                }
            }
        }
    }
    let scripts = project.and_then(|p| p.get("scripts")).or_else(|| poetry.and_then(|p| p.get("scripts"))).and_then(|s| s.as_table());
    if let Some(scripts) = scripts {
        for (name, target) in scripts {
            let Some(target) = target.as_str() else { continue };
            facts.commands.push(CommandFact {
                name: name.clone(),
                command: target.to_owned(),
                line: line_of(text, &format!("{name} =")),
                runs_files: Vec::new(),
                runs_commands: Vec::new(),
                entry_point: Some(target.to_owned()),
            });
        }
    }
}

/// `site-packages` of a virtual environment in the project directory or repository root.
fn site_packages(root: &Path, dir: &str) -> Option<PathBuf> {
    let bases = [if dir.is_empty() { root.to_path_buf() } else { root.join(dir) }, root.to_path_buf()];
    for base in bases {
        for venv in [".venv", "venv", "env"] {
            let lib = base.join(venv).join("lib");
            let Ok(entries) = std::fs::read_dir(&lib) else { continue };
            for entry in entries.flatten() {
                let candidate = entry.path().join("site-packages");
                if entry.file_name().to_string_lossy().starts_with("python") && candidate.is_dir() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// The installed version (from the `.dist-info` directory) and source (its first top-level module)
/// of distribution `name` in `site`.
fn pypi_installed(site: &Path, name: &str) -> (Option<String>, Option<String>) {
    let normalized = name.replace(['-', '.'], "_").to_lowercase();
    let Ok(entries) = std::fs::read_dir(site) else { return (None, None) };
    let mut version = None;
    let mut top_level: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = file.strip_suffix(".dist-info")
            && let Some((dist, ver)) = stem.rsplit_once('-')
            && dist.replace(['-', '.'], "_").to_lowercase() == normalized
        {
            version = Some(ver.to_owned());
            if let Ok(t) = std::fs::read_to_string(entry.path().join("top_level.txt")) {
                top_level = t.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect();
            }
        }
    }
    let module = top_level.first().cloned().unwrap_or(normalized);
    let dir = site.join(&module);
    let source = if dir.is_dir() {
        Some(dir.to_string_lossy().into_owned())
    } else if site.join(format!("{module}.py")).is_file() {
        Some(site.join(format!("{module}.py")).to_string_lossy().into_owned())
    } else {
        None
    };
    (version, source)
}
