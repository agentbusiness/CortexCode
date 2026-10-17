use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub name: String,
    pub port: u16,
    pub retries: u32,
}

#[derive(Debug, PartialEq)]
pub enum ConfigError {
    Io(String),
    Syntax { line: usize },
    Missing(&'static str),
    Invalid { key: String, value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "cannot read config: {e}"),
            ConfigError::Syntax { line } => write!(f, "line {line}: expected key = value"),
            ConfigError::Missing(key) => write!(f, "missing key {key}"),
            ConfigError::Invalid { key, value } => write!(f, "invalid value {value:?} for {key}"),
        }
    }
}

/// Parse `key = value` lines; `#` starts a comment.
pub fn parse_config(text: &str) -> Result<Config, ConfigError> {
    let mut name = None;
    let mut port = None;
    let mut retries = 1;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(ConfigError::Syntax { line: i + 1 })?;
        let (key, value) = (key.trim(), value.trim());
        let invalid = || ConfigError::Invalid { key: key.to_string(), value: value.to_string() };
        match key {
            "name" => name = Some(value.to_string()),
            "port" => port = Some(value.parse().map_err(|_| invalid())?),
            "retries" => retries = value.parse().map_err(|_| invalid())?,
            _ => {}
        }
    }
    Ok(Config { name: name.ok_or(ConfigError::Missing("name"))?, port: port.ok_or(ConfigError::Missing("port"))?, retries })
}

/// Read and parse the configuration file at `path`.
pub fn load_from_file(path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    parse_config(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys() {
        let cfg = parse_config("name = a\nport = 80\nretries = 2\n").unwrap();
        assert_eq!(cfg, Config { name: "a".into(), port: 80, retries: 2 });
    }

    #[test]
    fn reports_missing_port() {
        assert_eq!(parse_config("name = a"), Err(ConfigError::Missing("port")));
    }

    #[test]
    fn reports_syntax_line() {
        assert_eq!(parse_config("name = a\nport"), Err(ConfigError::Syntax { line: 2 }));
    }
}
