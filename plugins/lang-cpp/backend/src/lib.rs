//! The C/C++ language pack (`lang-cpp`): C and C++ sources and headers and `CMakeLists.txt` for the
//! code graph, clangd for diagnostics and navigation (pointed at the workspace's
//! `compile_commands.json`), CMake / ctest / make verifiers, and the C/C++ skills in `data/skills`.

pub mod cmake;
pub mod cpp;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{Diagnostic, FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::{Launch, ServerSpec};
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::Path;

/// The C/C++ pack.
pub struct CppPack;

impl CppPack {
    /// The pack; CortexCode's home comes with every call (`PackEnv`), so it is not kept.
    pub fn new(_home: &Path) -> Self {
        Self
    }
}

/// The probe's sample: items of every kind the extractor names.
pub(crate) const SAMPLE: &str = r#"#include <map>
#include <string>

#define STORE_LIMIT 100

namespace store {

/// Keeps items.
class Store {
public:
    int get(const std::string& key) const;

private:
    std::map<std::string, int> items_;
};

enum class Kind { Small, Large };

int Store::get(const std::string& key) const {
    auto it = items_.find(key);
    return it == items_.end() ? 0 : it->second;
}

}  // namespace store
"#;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for CppPack {
    fn id(&self) -> &str {
        "cpp"
    }

    fn name(&self) -> &str {
        "C/C++"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// `cpp` and `c` exactly as the memory plugin's `Lang::Cpp` / `Lang::C` (code-graph ids stay the
    /// same: `.h` is C++, the grammar parses C declarations too), and CMake's project files.
    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo { name: "cpp".into(), sid_prefix: "cpp".into(), extensions: strings(&["cc", "cpp", "cxx", "hpp", "hh", "hxx", "h"]), ..LanguageInfo::default() },
            LanguageInfo { name: "c".into(), sid_prefix: "c".into(), extensions: strings(&["c"]), ..LanguageInfo::default() },
            LanguageInfo { name: "cmake".into(), sid_prefix: "cmake".into(), filenames: strings(&["CMakeLists.txt"]), ..LanguageInfo::default() },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "cpp" | "c" => cpp::extract(&file.text, facts),
            "cmake" => cmake::extract(&file.path, &file.text, ctx, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "src/store.cpp".into(),
            text: SAMPLE.into(),
            expect: strings(&["STORE_LIMIT", "store", "store::Store", "store::Store::get", "store::Store::items_", "store::Kind", "store::Kind::Large"]),
        }
    }

    fn verifiers(&self) -> Vec<Verifier> {
        let configure_build = format!("{} && cmake --build build", server::CMAKE_EXPORT);
        vec![
            Verifier {
            files: Vec::new(),                name: "cmake build".into(),
                kind: "build".into(),
                command: strings(&["sh", "-c", &configure_build]),
                when: strings(&["CMakeLists.txt"]),
                requires: strings(&["cmake"]),
            },
            Verifier {
            files: Vec::new(),                name: "ctest".into(),
                kind: "test".into(),
                command: strings(&["sh", "-c", &format!("{configure_build} && ctest --test-dir build --output-on-failure")]),
                when: strings(&["CMakeLists.txt"]),
                requires: strings(&["cmake", "ctest"]),
            },
            Verifier { files: Vec::new(),name: "make".into(), kind: "build".into(), command: strings(&["make"]), when: strings(&["Makefile", "makefile", "GNUmakefile"]), requires: strings(&["make"]) },
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec())
    }

    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        server::toolchain(&env.toolchains)
    }

    fn locate_server(&self, env: &PackEnv, _root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, &server::Host::current())
    }

    /// clangd pointed at the root's `compile_commands.json`.
    fn server_launch(&self, _env: &PackEnv, root: &Path, _server: &FoundServer) -> Result<Launch, String> {
        Ok(server::launch(root))
    }

    /// The missing-database warning.
    fn annotate_diagnostics(&self, _env: &PackEnv, root: &Path, files: &[String], diagnostics: &mut Vec<Diagnostic>) {
        server::annotate(root, files, diagnostics);
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, &server::Host::current(), log)
    }
}
