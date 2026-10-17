//! The shell language pack (`lang-shell`): sh / bash / zsh scripts for the code graph,
//! bash-language-server (with ShellCheck) for diagnostics and navigation, `shellcheck` and `bash -n`
//! as verifiers, and the shell skills in `data/skills`.

pub mod server;
pub mod shell;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::install::Toolchains;
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::{Path, PathBuf};

/// The pack, working in CortexCode's home (the server's settings name the installed ShellCheck).
pub struct ShellPack {
    home: PathBuf,
}

impl ShellPack {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }
}

/// The probe's sample: functions, variables, sourcing and commands.
const SAMPLE: &str = r#"#!/usr/bin/env bash
# Build the release.
set -euo pipefail

export OUT_DIR="${OUT_DIR:-dist}"
readonly VERSION="1.0.0"

source ./scripts/common.sh

# Prints a step.
log_step() {
  printf '==> %s\n' "$1"
}

build() {
  log_step "build $VERSION"
  mkdir -p "$OUT_DIR"
  ./scripts/compile.sh "$OUT_DIR"
}

function clean {
  rm -rf "$OUT_DIR"
}

case "${1:-build}" in
  clean) clean ;;
  *) build ;;
esac
"#;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for ShellPack {
    fn id(&self) -> &str {
        "shell"
    }

    fn name(&self) -> &str {
        "Shell"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        vec![LanguageInfo {
            name: "shell".into(),
            sid_prefix: "sh".into(),
            extensions: strings(&["sh", "bash", "zsh"]),
            shebangs: strings(&["sh", "bash", "zsh", "dash", "ksh"]),
            ..LanguageInfo::default()
        }]
    }

    fn extract(&self, language: &str, file: &SourceFile, _ctx: &ExtractContext, facts: &mut FileFacts) {
        if language == "shell" {
            shell::extract(&file.text, facts);
        }
    }

    fn sample(&self) -> Sample {
        Sample { path: "scripts/release.sh".into(), text: SAMPLE.into(), expect: strings(&["OUT_DIR", "VERSION", "log_step", "build", "clean"]) }
    }

    /// Shell scripts have no project marker: the verifiers apply to the scripts being checked
    /// (`{file}` one script, `{files}` every script checked).
    fn verifiers(&self) -> Vec<Verifier> {
        vec![
            Verifier { files: Vec::new(),name: "shellcheck".into(), kind: "lint".into(), command: strings(&[server::SHELLCHECK, "-x", "{files}"]), when: Vec::new(), requires: strings(&[server::SHELLCHECK]) },
            Verifier { files: Vec::new(),name: "bash -n".into(), kind: "syntax".into(), command: strings(&["bash", "-n", "{file}"]), when: Vec::new(), requires: strings(&["bash"]) },
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec_for(&Toolchains::new(&self.home)))
    }

    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        server::toolchain(&env.toolchains)
    }

    fn locate_server(&self, env: &PackEnv, root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, root)
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, log)
    }
}
