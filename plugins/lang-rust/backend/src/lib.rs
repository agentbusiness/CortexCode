//! The Rust language pack (`lang-rust`): `.rs` files and `Cargo.toml` manifests for the code graph,
//! rust-analyzer for diagnostics and navigation, cargo check / test / clippy as verifiers, and the
//! Rust skills in `data/skills`.

pub mod cargo;
pub mod rust;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::Path;

pub struct RustPack;

/// The probe's sample: items of every kind the extractor names.
const SAMPLE: &str = r#"//! Sample.
use std::collections::HashMap;

/// A store.
pub struct Store {
    items: HashMap<String, u32>,
}

pub trait Get {
    fn get(&self, key: &str) -> Option<u32>;
}

impl Get for Store {
    fn get(&self, key: &str) -> Option<u32> {
        self.items.get(key).copied()
    }
}

pub enum Kind {
    A,
    B(u32),
}

pub fn make() -> Store {
    Store { items: HashMap::new() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn makes() {
        assert!(super::make().get("x").is_none());
    }
}
"#;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for RustPack {
    fn id(&self) -> &str {
        "rust"
    }

    fn name(&self) -> &str {
        "Rust"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo { name: "rust".into(), sid_prefix: "rs".into(), extensions: strings(&["rs"]), ..LanguageInfo::default() },
            LanguageInfo { name: "cargo.toml".into(), sid_prefix: "cargo".into(), filenames: strings(&["Cargo.toml"]), ..LanguageInfo::default() },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "rust" => rust::extract(&file.text, facts),
            "cargo.toml" => cargo::extract(&file.path, &file.text, ctx, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "src/lib.rs".into(),
            text: SAMPLE.into(),
            expect: strings(&["Store", "Store::items", "Get", "Get::get", "Store::get", "Kind", "Kind::B", "make", "tests", "tests::makes"]),
        }
    }

    fn verifiers(&self) -> Vec<Verifier> {
        let cargo = |name: &str, kind: &str, args: &[&str]| Verifier { files: Vec::new(),name: name.into(), kind: kind.into(), command: strings(args), when: strings(&["Cargo.toml"]), requires: strings(&["cargo"]) };
        vec![
            cargo("cargo check", "build", &["cargo", "check", "--message-format=short"]),
            cargo("cargo test", "test", &["cargo", "test"]),
            cargo("cargo clippy", "lint", &["cargo", "clippy", "--", "-D", "warnings"]),
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec())
    }

    fn locate_server(&self, _env: &PackEnv, root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(root)
    }

    fn install_server(&self, _env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(log)
    }
}
