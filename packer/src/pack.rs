//! Building and packing a plugin folder.

use cc_protocol::contracts::frame::package::{
    BuildInfo, CcPluginManifest, FORMAT, PACKAGE_MANIFEST, PLUGIN_TOML, PackedBackend, PackedShell, PackedSource, PackedUi, PluginToml,
    safe_relative,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub dir: PathBuf,
    pub out: Option<PathBuf>,
    /// Include the plugin's source (`source/`).
    pub source: bool,
    /// Run the build commands first.
    pub run_build: bool,
    /// Keep plugin.toml's version as it is (a release); otherwise the package is a dev build,
    /// versioned `<version>+dev.<UTC date>.<time>`, so every build is its own installed version.
    pub release: bool,
    /// Version the package `<version>+<id>` (a digest of the plugin's source, from the app builder).
    pub build_id: Option<String>,
}

#[derive(Debug)]
pub struct Packed {
    pub path: PathBuf,
    pub bytes: u64,
    pub manifest: CcPluginManifest,
}

/// `<version>+dev.YYYYMMDD.HHMMSS` (UTC) for a dev build.
fn dev_version(version: &str) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let base = version.split('+').next().unwrap_or(version);
    format!("{base}+dev.{year:04}{month:02}{day:02}.{:02}{:02}{:02}", rest / 3600, rest / 60 % 60, rest % 60)
}

/// A file to put in the package: where it comes from and whether it is executable.
struct Entry {
    from: PathBuf,
    executable: bool,
}

pub fn read_plugin_toml(dir: &Path) -> Result<PluginToml, String> {
    let path = dir.join(PLUGIN_TOML);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let toml: PluginToml = toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    toml.validate().map_err(|e| format!("invalid {}: {e}", path.display()))?;
    Ok(toml)
}

fn run(dir: &Path, command: &[String], what: &str) -> Result<(), String> {
    let (program, args) = command.split_first().ok_or_else(|| format!("{what}: empty build command"))?;
    eprintln!("cortex-pack: {what}: {} (in {})", command.join(" "), dir.display());
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_remove("NODE_ENV")
        .status()
        .map_err(|e| format!("{what}: cannot run {program}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what}: `{}` failed ({status})", command.join(" "))) }
}

fn output(program: &str, args: &[&str], dir: &Path) -> Option<String> {
    let out = Command::new(program).args(args).current_dir(dir).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The Rust host target (`rustc -vV`), which is what `cargo build` builds for.
fn rust_target(dir: &Path) -> Result<(String, String), String> {
    let verbose = output("rustc", &["-vV"], dir).ok_or("cannot run rustc -vV")?;
    let host = verbose.lines().find_map(|line| line.strip_prefix("host: ")).ok_or("rustc -vV printed no host")?.trim().to_owned();
    let version = verbose.lines().next().unwrap_or_default().to_owned();
    Ok((host, version))
}

/// Add every regular file under `dir` as `<prefix>/<relative>`, skipping `skip` directory names.
/// Symbolic links are refused (a package holds only files and directories).
fn add_tree(entries: &mut BTreeMap<String, Entry>, dir: &Path, prefix: &str, skip: &[&str]) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let read = std::fs::read_dir(&current).map_err(|e| format!("{}: {e}", current.display()))?;
        for entry in read {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                if !skip.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if kind.is_file() {
                let relative = path.strip_prefix(dir).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
                let mode = entry.metadata().map_err(|e| e.to_string())?.permissions().mode();
                add_file(entries, &format!("{prefix}/{relative}"), path, mode & 0o100 != 0)?;
            } else if kind.is_symlink() {
                return Err(format!("{} is a symbolic link; a package holds only files and directories", path.display()));
            }
        }
    }
    Ok(())
}

fn add_file(entries: &mut BTreeMap<String, Entry>, name: &str, from: PathBuf, executable: bool) -> Result<(), String> {
    if !safe_relative(name) || name == PACKAGE_MANIFEST {
        return Err(format!("invalid package path `{name}`"));
    }
    if entries.insert(name.to_owned(), Entry { from, executable }).is_some() {
        return Err(format!("`{name}` would be packed twice"));
    }
    Ok(())
}

/// The plugin's source files: what git tracks or would track in the folder (honouring
/// .gitignore), or, outside a git repository, every file but build outputs and installs.
fn source_files(dir: &Path) -> Result<Vec<String>, String> {
    if let Some(listing) = output("git", &["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "."], dir) {
        let mut files: Vec<String> = listing.split('\0').filter(|f| !f.is_empty()).map(str::to_owned).collect();
        // A tracked file deleted in the working tree is not part of the source any more.
        files.retain(|f| dir.join(f).is_file());
        files.sort();
        files.dedup();
        return Ok(files);
    }
    let mut entries = BTreeMap::new();
    add_tree(&mut entries, dir, "x", &["target", "node_modules", "dist", ".git"])?;
    Ok(entries.into_keys().map(|k| k.trim_start_matches("x/").to_owned()).collect())
}

/// The last path component of a package or module id after the plugin id (`terminal.pane` -> `pane`).
fn short_name(id: &str, plugin: &str) -> String {
    id.strip_prefix(&format!("{plugin}.")).unwrap_or(id).replace(['/', '\\'], "_")
}

pub fn build(options: &Options) -> Result<Packed, String> {
    let dir = std::fs::canonicalize(&options.dir).map_err(|e| format!("{}: {e}", options.dir.display()))?;
    let toml = read_plugin_toml(&dir)?;
    let mut meta = toml.plugin.clone();
    if let Some(id) = &options.build_id {
        let base = meta.version.split('+').next().unwrap_or(&meta.version).to_owned();
        meta.version = format!("{base}+{id}");
    } else if !options.release {
        meta.version = dev_version(&meta.version);
    }
    let (target, rustc) = rust_target(&dir)?;
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    add_file(&mut entries, PLUGIN_TOML, dir.join(PLUGIN_TOML), false)?;
    // The plugin's icon travels as `icon.<ext>`.
    if let Some(icon) = &meta.icon {
        let from = dir.join(icon);
        if !from.is_file() {
            return Err(format!("[plugin] icon {} does not exist", from.display()));
        }
        let extension = from.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let packed = if extension.is_empty() { "icon".to_owned() } else { format!("icon.{extension}") };
        add_file(&mut entries, &packed, from, false)?;
        meta.icon = Some(packed);
    }

    // The backend: build, then the executable and its assets beside it in backend/.
    let mut backend = None;
    if let Some(spec) = &toml.backend {
        let backend_dir = dir.join(&spec.dir);
        if options.run_build && !spec.build.is_empty() {
            run(&backend_dir, &spec.build, "backend build")?;
        }
        let exec_rel = spec.exec.as_deref().ok_or("[backend] exec is required")?;
        let exec = dir.join(exec_rel);
        let metadata = std::fs::metadata(&exec).map_err(|e| format!("the backend executable {}: {e}", exec.display()))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("{} is not an executable file", exec.display()));
        }
        let exec_name = exec.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("the executable has no name")?;
        let packed_exec = format!("backend/{exec_name}");
        add_file(&mut entries, &packed_exec, exec.clone(), true)?;
        let mut assets = Vec::new();
        for asset in &spec.assets {
            let from = dir.join(asset);
            let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| format!("asset `{asset}` has no name"))?;
            let packed = format!("backend/{name}");
            if from.is_dir() {
                add_tree(&mut entries, &from, &packed, &[])?;
            } else if from.is_file() {
                let mode = std::fs::metadata(&from).map_err(|e| e.to_string())?.permissions().mode();
                add_file(&mut entries, &packed, from.clone(), mode & 0o100 != 0)?;
            } else {
                return Err(format!("asset {} does not exist (did the backend build make it?)", from.display()));
            }
            assets.push(packed);
        }
        backend = Some(PackedBackend {
            category: spec.category.clone().unwrap_or_default(),
            contract: spec.contract.clone().unwrap_or_default(),
            exec: packed_exec,
            args: spec.args.clone(),
            config: spec.config.clone().unwrap_or(serde_json::Value::Null),
            swap: spec.swap.clone(),
            assets,
            hello_timeout_ms: spec.hello_timeout_ms,
            probe_timeout_ms: spec.probe_timeout_ms,
            machine: spec.machine.clone(),
        });
    }

    // The UI: install when needed, build, then every package's bundle in ui/<name>/.
    let mut ui = Vec::new();
    let mut node = String::new();
    if let Some(spec) = &toml.ui {
        let root = dir.join(&spec.dir);
        if options.run_build {
            if !root.join("node_modules/.package-lock.json").is_file() {
                run(&root, &["npm".into(), "ci".into(), "--prefer-offline".into(), "--no-audit".into(), "--no-fund".into()], "UI install")?;
            }
            if !spec.build.is_empty() {
                run(&root, &spec.build, "UI build")?;
            }
        }
        node = output("node", &["--version"], &root).unwrap_or_default();
        for package in &spec.package {
            let dist = dir.join(package.dist_dir());
            let bundle: serde_json::Value = std::fs::read(dist.join("manifest.json"))
                .map_err(|e| format!("UI package `{}` is not built ({}): {e}", package.id, dist.join("manifest.json").display()))
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", dist.join("manifest.json").display())))?;
            if bundle["id"].as_str() != Some(package.id.as_str()) || bundle["slot"].as_str() != Some(package.slot.as_str()) {
                return Err(format!("{} says {} on {}; plugin.toml says {} on {}", dist.join("manifest.json").display(), bundle["id"], bundle["slot"], package.id, package.slot));
            }
            let packed = format!("ui/{}", short_name(&package.id, &meta.id));
            add_tree(&mut entries, &dist, &packed, &[])?;
            ui.push(PackedUi { id: package.id.clone(), slot: package.slot.clone(), dir: packed, version: bundle["version"].as_str().unwrap_or_default().to_owned() });
        }
    }

    // Shell modules (their own node_modules only when they declare runtime dependencies).
    let mut shell = Vec::new();
    for module in &toml.shell {
        let from = dir.join(&module.dir);
        let package: serde_json::Value = std::fs::read(from.join("package.json"))
            .map_err(|e| format!("shell module `{}`: {e}", module.id))
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("shell module `{}`: {e}", module.id)))?;
        let has_dependencies = package["dependencies"].as_object().is_some_and(|deps| !deps.is_empty());
        let skip: &[&str] = if has_dependencies { &[".git"] } else { &["node_modules", ".git"] };
        let packed = format!("shell/{}", short_name(&module.id, &meta.id));
        add_tree(&mut entries, &from, &packed, skip)?;
        shell.push(PackedShell { id: module.id.clone(), dir: packed, serves: module.serves.clone() });
    }

    // The source, for self-rewrite.
    let source = match &toml.source {
        Some(spec) => {
            let included = if options.source {
                for file in source_files(&dir)? {
                    let from = dir.join(&file);
                    let mode = std::fs::metadata(&from).map_err(|e| e.to_string())?.permissions().mode();
                    add_file(&mut entries, &format!("source/{file}"), from, mode & 0o100 != 0)?;
                }
                Some("source".to_owned())
            } else {
                None
            };
            Some(PackedSource { repo: spec.repo.clone(), subdir: spec.subdir.clone(), url: spec.url.clone(), dir: included })
        }
        None if options.source => return Err("plugin.toml has no [source]; pack with --no-source or add one".into()),
        None => None,
    };

    let git_commit = output("git", &["rev-parse", "HEAD"], &dir).map(|head| {
        let dirty = output("git", &["status", "--porcelain", "--", "."], &dir).is_some_and(|s| !s.is_empty());
        if dirty { format!("{head}-dirty") } else { head }
    });
    let manifest = CcPluginManifest {
        format: FORMAT.into(),
        plugin: meta.clone(),
        backend,
        ui,
        shell,
        source,
        build: BuildInfo {
            target: target.clone(),
            packer: format!("cortex-pack {}", env!("CARGO_PKG_VERSION")),
            built_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            rustc,
            node,
            protocol: cc_protocol::PROTOCOL_VERSION.into(),
            git_commit,
        },
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    // A dev build overwrites the folder's one dev package; a release is named by its version.
    let out = match &options.out {
        Some(out) => out.clone(),
        None if options.release => dir.join("target").join(format!("{}-{}-{target}.ccplugin", meta.id, meta.version)),
        None => dir.join("target").join(format!("{}-dev-{target}.ccplugin", meta.id)),
    };
    write_archive(&out, &manifest_bytes, &entries)?;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(Packed { path: out, bytes, manifest })
}

/// Write the archive atomically: manifest.json first, then the files in path order, with
/// normalized headers (mode 0644/0755, no owner, mtime 0).
fn write_archive(out: &Path, manifest: &[u8], entries: &BTreeMap<String, Entry>) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = out.with_extension(format!("ccplugin.tmp-{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(std::io::BufWriter::new(file), flate2::Compression::default()));
        let header = |size: u64, executable: bool| {
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(if executable { 0o755 } else { 0o644 });
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            header
        };
        let mut head = header(manifest.len() as u64, false);
        builder.append_data(&mut head, PACKAGE_MANIFEST, manifest).map_err(|e| e.to_string())?;
        for (path, entry) in entries {
            let file = std::fs::File::open(&entry.from).map_err(|e| format!("{}: {e}", entry.from.display()))?;
            let size = file.metadata().map_err(|e| e.to_string())?.len();
            let mut head = header(size, entry.executable);
            builder.append_data(&mut head, path, file).map_err(|e| format!("{path}: {e}"))?;
        }
        let encoder = builder.into_inner().map_err(|e| e.to_string())?;
        let mut writer = encoder.finish().map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        Ok(())
    })();
    match result {
        Ok(()) => std::fs::rename(&tmp, out).map_err(|e| format!("{}: {e}", out.display())),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}
