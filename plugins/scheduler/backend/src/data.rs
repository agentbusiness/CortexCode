//! The plugin's short texts (`data/texts.toml`): the shipped copy compiled in, the live copy
//! (`backend/data/texts.toml` in the plugin's live folder) read on every use, so an edit there
//! takes effect at once. A live file that does not parse, or drops a placeholder, is ignored.

use std::collections::BTreeMap;
use std::path::PathBuf;

const SHIPPED: &str = include_str!("../data/texts.toml");

fn table(text: &str) -> Option<BTreeMap<String, String>> {
    toml::from_str::<BTreeMap<String, String>>(text).ok()
}

fn placeholders(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = text.split('{').skip(1).filter_map(|part| part.split_once('}').map(|(name, _)| name)).collect();
    out.sort_unstable();
    out
}

pub struct Texts {
    live: Option<PathBuf>,
    shipped: BTreeMap<String, String>,
}

impl Texts {
    pub fn new(live_dir: Option<PathBuf>) -> Self {
        Self { live: live_dir.map(|dir| dir.join("backend").join("data").join("texts.toml")), shipped: table(SHIPPED).unwrap_or_default() }
    }

    /// The text `key` with `{name}`s filled in.
    pub fn get(&self, key: &str, vars: &[(&str, &str)]) -> String {
        let shipped = self.shipped.get(key).cloned().unwrap_or_default();
        let live = self
            .live
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| table(&text))
            .and_then(|table| table.get(key).cloned())
            .filter(|text| placeholders(text) == placeholders(&shipped));
        let mut out = live.unwrap_or(shipped);
        for (name, value) in vars {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }
}
