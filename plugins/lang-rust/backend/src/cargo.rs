//! Cargo.toml: the package name, dependencies by section (with the locked version and the
//! installed source directory when the repository is known), binaries, the build script and
//! `[package.metadata.scripts]` commands.
//!
//! Ported from the memory plugin's manifest extractor (the Cargo part of
//! plugins/memory-irongraph/backend/src/lang/manifest.rs), producing `lang@1` facts. What lives on
//! disk (Cargo.lock, registry sources, the files binaries and the build script are) is resolved only
//! when the extraction context names the repository root.

use cc_lang_kit::ExtractContext;
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts};
use cc_lang_kit::manifest::{command_targets, dir_of, is_repo_file, join_rel, line_of};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Cargo registry source roots (`$CARGO_HOME/registry/src/*`), read once.
fn registry_roots() -> &'static [PathBuf] {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let Some(home) = cc_lang_kit::install::cargo_home() else { return Vec::new() };
        let Ok(entries) = std::fs::read_dir(home.join("registry").join("src")) else { return Vec::new() };
        entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
    })
}

pub fn extract(path: &str, text: &str, ctx: &ExtractContext, facts: &mut FileFacts) {
    extract_with(path, text, ctx.root.as_deref(), registry_roots(), facts);
}

/// [`extract`] with explicit registry source roots.
pub fn extract_with(path: &str, text: &str, root: Option<&Path>, cargo_registry: &[PathBuf], facts: &mut FileFacts) {
    let Ok(doc) = text.parse::<toml::Table>() else {
        facts.parse_errors = 1;
        return;
    };
    let dir = dir_of(path);
    let package = doc.get("package").and_then(|p| p.as_table());
    facts.package = package.and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(str::to_owned);
    let lock = root.and_then(|root| find_cargo_lock(root, dir));
    let mut sections: Vec<(String, &toml::Table)> = Vec::new();
    for (key, section) in [("dependencies", "dependencies"), ("dev-dependencies", "dev"), ("build-dependencies", "build")] {
        if let Some(t) = doc.get(key).and_then(|d| d.as_table()) {
            sections.push((section.to_owned(), t));
        }
    }
    if let Some(ws) = doc.get("workspace").and_then(|w| w.as_table())
        && let Some(t) = ws.get("dependencies").and_then(|d| d.as_table())
    {
        sections.push(("workspace".to_owned(), t));
    }
    if let Some(targets) = doc.get("target").and_then(|t| t.as_table()) {
        for target in targets.values().filter_map(|v| v.as_table()) {
            for (key, section) in [("dependencies", "dependencies"), ("dev-dependencies", "dev"), ("build-dependencies", "build")] {
                if let Some(t) = target.get(key).and_then(|d| d.as_table()) {
                    sections.push((section.to_owned(), t));
                }
            }
        }
    }
    for (section, table) in sections {
        for (name, value) in table {
            let (spec, crate_name, path_dep) = match value {
                toml::Value::String(v) => (v.clone(), name.clone(), None),
                toml::Value::Table(t) => {
                    let spec = t.get("version").and_then(|v| v.as_str()).map(str::to_owned).unwrap_or_else(|| {
                        if t.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
                            "workspace".into()
                        } else if t.contains_key("path") {
                            "path".into()
                        } else if t.contains_key("git") {
                            "git".into()
                        } else {
                            String::new()
                        }
                    });
                    let crate_name = t.get("package").and_then(|p| p.as_str()).unwrap_or(name).to_owned();
                    (spec, crate_name, t.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                }
                _ => continue,
            };
            let mut installed_version = None;
            let mut source_dir = None;
            if let Some(path_dep) = path_dep {
                if let Some(root) = root {
                    let abs = if dir.is_empty() { root.join(&path_dep) } else { root.join(dir).join(&path_dep) };
                    source_dir = Some(abs.to_string_lossy().into_owned());
                }
            } else if let Some(locked) = lock.as_deref().and_then(|lock| lock_package(lock, package, &crate_name, &spec, section == "workspace")) {
                // Cargo uses these two source-directory identities for crates.io. A sole folder
                // from an alternate registry is still not evidence that it supplied this crate.
                if locked.source == "registry+https://github.com/rust-lang/crates.io-index" {
                    let mut sources = cargo_registry.iter().filter(|registry| {
                        matches!(registry.file_name().and_then(|n| n.to_str()), Some("index.crates.io-1949cf8c6b5b557f" | "github.com-1ecc6299db9ec823"))
                    }).map(|registry| registry.join(format!("{crate_name}-{}", locked.version))).filter(|p| p.is_dir());
                    if let Some(candidate) = sources.next()
                        && sources.next().is_none() {
                        source_dir = Some(candidate.to_string_lossy().into_owned());
                    }
                }
                installed_version = Some(locked.version);
            }
            facts.dependencies.push(DependencyFact { name: crate_name, ecosystem: "cargo".into(), spec, section: section.clone(), installed_version, source_dir });
        }
    }
    // Binaries and build scripts are the commands a crate exposes.
    if let Some(bins) = doc.get("bin").and_then(|b| b.as_array()) {
        for bin in bins.iter().filter_map(|b| b.as_table()) {
            let Some(name) = bin.get("name").and_then(|n| n.as_str()) else { continue };
            let file = bin.get("path").and_then(|p| p.as_str()).map(str::to_owned).unwrap_or_else(|| format!("src/bin/{name}.rs"));
            let rel = join_rel(dir, &file);
            let runs_files = if is_repo_file(root, &rel) { vec![rel] } else { Vec::new() };
            facts.commands.push(CommandFact {
                name: format!("bin:{name}"),
                command: format!("cargo run --bin {name}"),
                line: line_of(text, &format!("name = \"{name}\"")),
                runs_files,
                runs_commands: Vec::new(),
                entry_point: None,
            });
        }
    }
    if let Some(pkg) = package {
        let main = join_rel(dir, "src/main.rs");
        if is_repo_file(root, &main) {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("main");
            facts.commands.push(CommandFact {
                name: format!("bin:{name}"),
                command: format!("cargo run --bin {name}"),
                line: line_of(text, "[package]"),
                runs_files: vec![main],
                runs_commands: Vec::new(),
                entry_point: None,
            });
        }
        let build = pkg.get("build").and_then(|b| b.as_str()).map(str::to_owned).unwrap_or_else(|| "build.rs".into());
        let build_rel = join_rel(dir, &build);
        if is_repo_file(root, &build_rel) {
            facts.commands.push(CommandFact {
                name: "build-script".into(),
                command: build.clone(),
                line: line_of(text, "[package]"),
                runs_files: vec![build_rel],
                runs_commands: Vec::new(),
                entry_point: None,
            });
        }
        // cargo-run-script convention: [package.metadata.scripts].
        if let Some(scripts) = pkg.get("metadata").and_then(|m| m.get("scripts")).and_then(|s| s.as_table()) {
            for (name, command) in scripts {
                let Some(command) = command.as_str() else { continue };
                let (runs_files, runs_commands) = command_targets(command, dir, root);
                facts.commands.push(CommandFact {
                    name: name.clone(),
                    command: command.to_owned(),
                    line: line_of(text, &format!("{name} =")),
                    runs_files,
                    runs_commands,
                    entry_point: None,
                });
            }
        }
    }
}

/// The nearest Cargo.lock from the manifest's directory up to the repository root.
fn find_cargo_lock(root: &Path, dir: &str) -> Option<String> {
    let mut current = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    loop {
        if let Ok(text) = std::fs::read_to_string(current.join("Cargo.lock")) {
            return Some(text);
        }
        if current == root || !current.pop() || !current.starts_with(root) {
            return None;
        }
    }
}

struct LockedPackage {
    version: String,
    source: String,
}

/// Resolve only a unique registry package satisfying Cargo's requirement and the owning
/// package's lock edges. A workspace dependency declaration has no single owning package.
fn lock_package(lock: &str, owner: Option<&toml::Table>, name: &str, spec: &str, workspace: bool) -> Option<LockedPackage> {
    let doc = lock.parse::<toml::Table>().ok()?;
    let packages: Vec<&toml::Table> = doc.get("package")?.as_array()?.iter().filter_map(|p| p.as_table()).collect();
    let requirement = match spec {
        "workspace" | "" => None,
        "git" | "path" => return None,
        _ => Some(semver::VersionReq::parse(spec).ok()?),
    };
    let edges = if workspace {
        None
    } else {
        let owner = owner?;
        let owner_name = owner.get("name")?.as_str()?;
        let owner_version = owner.get("version").and_then(|v| v.as_str());
        let mut owners = packages.iter().filter(|p| {
            p.get("name").and_then(|v| v.as_str()) == Some(owner_name)
                && p.get("source").is_none()
                && owner_version.is_none_or(|version| p.get("version").and_then(|v| v.as_str()) == Some(version))
        });
        let owner = owners.next()?;
        if owners.next().is_some() { return None; }
        Some(owner.get("dependencies")?.as_array()?)
    };
    let mut candidates = packages.iter().filter(|package| {
        let version = package.get("version").and_then(|v| v.as_str());
        let source = package.get("source").and_then(|v| v.as_str());
        if package.get("name").and_then(|v| v.as_str()) != Some(name)
            || !source.is_some_and(|s| s.starts_with("registry+")) {
            return false;
        }
        let Some(version) = version else { return false };
        let Ok(parsed) = semver::Version::parse(version) else { return false };
        if !requirement.as_ref().is_none_or(|req| req.matches(&parsed)) { return false; }
        edges.is_none_or(|edges| edges.iter().filter_map(|v| v.as_str()).any(|edge| {
            // Cargo.lock uses `name`, `name version`, or `name version (source)`.
            let mut parts = edge.splitn(3, ' ');
            parts.next() == Some(name)
                && parts.next().is_none_or(|v| v == version)
                && parts.next().is_none_or(|s| s.strip_prefix('(').and_then(|s| s.strip_suffix(')')) == source)
        }))
    });
    let candidate = candidates.next()?;
    if candidates.next().is_some() { return None; }
    Some(LockedPackage {
        version: candidate.get("version")?.as_str()?.to_owned(),
        source: candidate.get("source")?.as_str()?.to_owned(),
    })
}
