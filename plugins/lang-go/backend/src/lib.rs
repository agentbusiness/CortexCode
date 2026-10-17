//! The Go language pack (`lang-go`): `.go` files and `go.mod` manifests for the code graph, gopls
//! for diagnostics and navigation, go build / vet / test as verifiers, Go, gopls and delve installed
//! into CortexCode's toolchains directory, and the Go skills in `data/skills`.

pub mod go;
pub mod gomod;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::install::Toolchains;
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::Path;

/// The Go pack, working in CortexCode's home (its toolchains directory holds Go, gopls and delve).
pub struct GoPack {
    toolchains: Toolchains,
}

impl GoPack {
    pub fn new(home: &Path) -> Self {
        Self { toolchains: Toolchains::new(home) }
    }
}

/// The probe's sample: items of every kind the extractor names.
pub(crate) const SAMPLE: &str = r#"// Package store keeps items.
package store

import (
	"errors"
	"sync"
)

// ErrMissing is returned for an unknown key.
var ErrMissing = errors.New("missing")

// Limit caps the item count.
const Limit = 100

// Store is a map safe for concurrent use.
type Store struct {
	mu    sync.Mutex
	items map[string]int
}

// Getter reads items.
type Getter interface {
	Get(key string) (int, error)
}

// New makes an empty store.
func New() *Store {
	return &Store{items: map[string]int{}}
}

// Get returns the item under key.
func (s *Store) Get(key string) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	v, ok := s.items[key]
	if !ok {
		return 0, ErrMissing
	}
	return v, nil
}
"#;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for GoPack {
    fn id(&self) -> &str {
        "go"
    }

    fn name(&self) -> &str {
        "Go"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// Names, prefixes and extensions as the memory plugin's `Lang::Go` (code-graph ids stay the same).
    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo { name: "go".into(), sid_prefix: "go".into(), extensions: strings(&["go"]), ..LanguageInfo::default() },
            LanguageInfo { name: "go.mod".into(), sid_prefix: "gomod".into(), filenames: strings(&["go.mod"]), ..LanguageInfo::default() },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "go" => go::extract(&file.path, &file.text, facts),
            "go.mod" => gomod::extract(&file.path, &file.text, ctx, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample { path: "store/store.go".into(), text: SAMPLE.into(), expect: strings(&["ErrMissing", "Limit", "Store", "Store.items", "Getter", "Getter.Get", "New", "Store.Get"]) }
    }

    fn verifiers(&self) -> Vec<Verifier> {
        let go = |name: &str, kind: &str, args: &[&str]| Verifier { files: Vec::new(),name: name.into(), kind: kind.into(), command: strings(args), when: strings(&["go.mod"]), requires: strings(&["go"]) };
        vec![go("go build", "build", &["go", "build", "./..."]), go("go vet", "lint", &["go", "vet", "./..."]), go("go test", "test", &["go", "test", "./..."])]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec(&self.toolchains, &server::Host::current()))
    }

    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        server::toolchain(&env.toolchains, &server::Host::current())
    }

    fn locate_server(&self, env: &PackEnv, root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, &server::Host::current(), root)
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, log)
    }
}
