//! Frame configuration (the app bundle's `Contents/Resources/cortex.toml`, from
//! `scripts/app/cortex.toml`, whose header documents the schema); unknown keys are rejected. Relative paths resolve against the config file.
//!
//! The frame runs only installed packages (`<home>/plugins/<id>/<version>/`); the config holds the
//! frame's own settings and, per plugin id, overrides of what its package declares: `args`,
//! timeouts and `[plugin.<id>.config]` keys laid over the package's `[backend.config]` defaults.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_HELLO_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    frame: RawFrame,
    #[serde(default)]
    plugin: BTreeMap<String, RawOverride>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFrame {
    home: Option<String>,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    ui: bool,
    ui_dir: Option<String>,
    /// The live folder: every plugin's data (prompts, texts, skills), where plugins read it live
    /// (default `<home>/live`).
    live: Option<String>,
    /// The app's prebuilt plugin packages (`<id>@<version>.ccplugin`), installed at start.
    packages: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOverride {
    args: Option<Vec<String>>,
    hello_timeout_ms: Option<u64>,
    probe_timeout_ms: Option<u64>,
    config: Option<toml::Table>,
}

/// The frame config's overrides for one plugin.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PluginOverride {
    pub args: Option<Vec<String>>,
    pub hello_timeout_ms: Option<u64>,
    pub probe_timeout_ms: Option<u64>,
    /// `[plugin.<id>.config]` as JSON (`null` when absent).
    pub config: Value,
}

/// The backend of one plugin as the supervisor runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfig {
    /// The plugin (unit) this backend belongs to.
    pub plugin: String,
    pub category: String,
    pub hello_timeout_ms: u64,
    pub probe_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory relative paths resolve against (and the plugins' working directory).
    pub base: PathBuf,
    /// `[frame] home`, unexpanded (`None` = default).
    pub home: Option<String>,
    pub port: u16,
    pub ui: bool,
    pub ui_dir: PathBuf,
    /// `[frame] live`, resolved against the config's directory (`None` = `<home>/live`).
    pub live: Option<PathBuf>,
    /// `[frame] packages`: the app's prebuilt plugin packages, resolved against the config's directory.
    pub packages: Option<PathBuf>,
    /// Overrides by plugin id.
    pub overrides: BTreeMap<String, PluginOverride>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid config {path}: {message}")]
    Invalid { path: String, message: String },
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let absolute = absolutize(path);
        let text = std::fs::read_to_string(&absolute)
            .map_err(|source| ConfigError::Read { path: absolute.display().to_string(), source })?;
        Self::parse(&text, &absolute)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid { path: path.display().to_string(), message };
        let raw: RawConfig = toml::from_str(text).map_err(|error| invalid(error.to_string()))?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let mut overrides = BTreeMap::new();
        for (id, entry) in raw.plugin {
            if !cc_protocol::contracts::frame::package::valid_id(&id) {
                return Err(invalid(format!("[plugin.{id}]: `{id}` is not a plugin id")));
            }
            let config = table_to_json(entry.config).map_err(|error| invalid(format!("[plugin.{id}.config]: {error}")))?;
            overrides.insert(
                id,
                PluginOverride { args: entry.args, hello_timeout_ms: entry.hello_timeout_ms, probe_timeout_ms: entry.probe_timeout_ms, config },
            );
        }
        Ok(Config {
            home: raw.frame.home,
            port: raw.frame.port,
            ui: raw.frame.ui,
            ui_dir: resolve(&base, raw.frame.ui_dir.as_deref().unwrap_or("ui")),
            live: raw.frame.live.as_deref().map(|live| resolve(&base, live)),
            packages: raw.frame.packages.as_deref().map(|packages| resolve(&base, packages)),
            overrides,
            base,
        })
    }
}

fn table_to_json(table: Option<toml::Table>) -> Result<Value, String> {
    match table {
        Some(table) => serde_json::to_value(table).map_err(|error| error.to_string()),
        None => Ok(Value::Null),
    }
}

/// Shallow merge: `over`'s keys replace `base`'s (either may be null).
pub fn merge_config(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            let mut merged = base.clone();
            for (key, value) in over {
                merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        (base, Value::Null) => base.clone(),
        (_, over) => over.clone(),
    }
}

/// Expand a leading `~` to `$HOME`.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Resolve `value` (possibly `~`-prefixed or relative) against `base`, lexically normalized.
pub fn resolve(base: &Path, value: &str) -> PathBuf {
    let expanded = expand_tilde(value);
    let joined = if expanded.is_absolute() { expanded } else { base.join(expanded) };
    normalize(&joined)
}

/// Make a path absolute against the current directory and normalize it lexically.
pub fn absolutize(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
    };
    normalize(&joined)
}

/// Lexical normalization: drop `.` and fold `..` (structural path rule, no filesystem access).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
