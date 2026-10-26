//! The Python language pack (`lang-python`): `.py` / `.pyi` files and `pyproject.toml` manifests for
//! the code graph, basedpyright for diagnostics and navigation, pytest / ruff / basedpyright as
//! verifiers, and the Python skills in `data/skills`.

pub mod pyproject;
pub mod python;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::{Launch, ServerSpec};
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::Path;

pub struct PythonPack;

/// The probe's sample: definitions of every kind the extractor names.
const SAMPLE: &str = r#""""Sample module."""
import os
from dataclasses import dataclass

LIMIT = 3


@dataclass
class Item:
    """An item."""
    name: str
    count: int = 0


class Store:
    def __init__(self, path: str = "store.db"):
        self.path = path
        self.items: dict[str, Item] = {}

    def get(self, key: str) -> Item | None:
        return self.items.get(key)

    @staticmethod
    def make() -> "Store":
        return Store(os.environ.get("STORE_PATH", "store.db"))


async def load(store: Store, *keys: str) -> list[Item]:
    return [item for key in keys if (item := store.get(key)) is not None]


def test_make():
    assert Store.make().get("x") is None
"#;

/// Marker files of a Python project whose tests pytest collects (`tests` is a directory).
const PYTEST_MARKERS: &[&str] = &["pyproject.toml", "setup.cfg", "pytest.ini", "tox.ini", "setup.py", "conftest.py", "tests"];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for PythonPack {
    fn id(&self) -> &str {
        "python"
    }

    fn name(&self) -> &str {
        "Python"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo {
                name: "python".into(),
                sid_prefix: "py".into(),
                extensions: strings(&["py", "pyi"]),
                shebangs: strings(&["python", "python3"]),
                ..LanguageInfo::default()
            },
            LanguageInfo {
                name: "pyproject.toml".into(),
                sid_prefix: "pypi".into(),
                filenames: strings(&["pyproject.toml"]),
                ..LanguageInfo::default()
            },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "python" => python::extract(&file.path, &file.text, facts),
            "pyproject.toml" => pyproject::extract(&file.path, &file.text, ctx, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "pkg/store.py".into(),
            text: SAMPLE.into(),
            expect: strings(&["LIMIT", "Item", "Item.name", "Item.count", "Store", "Store.__init__", "Store.path", "Store.items", "Store.get", "Store.make", "load", "test_make"]),
        }
    }

    /// `{file}` in a command is the file being checked; `requires` names what must be on PATH.
    fn caches(&self) -> Vec<String> {
        vec![".venv".to_owned(), "venv".to_owned(), "__pycache__".to_owned(), ".pytest_cache".to_owned(), ".mypy_cache".to_owned(), ".ruff_cache".to_owned()]
    }

    fn verifiers(&self) -> Vec<Verifier> {
        let verifier = |name: &str, kind: &str, args: &[&str], when: &[&str], requires: &str| Verifier {
            files: Vec::new(),            name: name.into(),
            kind: kind.into(),
            command: strings(args),
            when: strings(when),
            requires: strings(&[requires]),
        };
        vec![
            // pytest exits 5 when it collects no test.
            Verifier { files: strings(&["**/test_*.py", "**/*_test.py"]), ..verifier("pytest", "test", &["python", "-m", "pytest", "-q"], PYTEST_MARKERS, "python") },
            verifier("pytest file", "test", &["python", "-m", "pytest", "-q", "-x", "{file}"], PYTEST_MARKERS, "python"),
            // The standard library's runner: what a project with a `tests` folder can always run
            // (pytest may not be installed in its environment).
            Verifier { files: strings(&["tests/test_*.py", "tests/**/test_*.py"]), ..verifier("unittest", "test", &["python", "-m", "unittest", "discover", "-s", "tests", "-q"], &["tests"], "python") },
            verifier("ruff", "lint", &["ruff", "check", "."], &["ruff.toml", ".ruff.toml", "pyproject.toml"], "ruff"),
            // Type check: errors only; the project's pyright configuration decides the rules. The
            // installed CLI is on commands' PATH (the toolchain).
            verifier("basedpyright", "typecheck", &[server::CLI, "--level", "error"], &["pyrightconfig.json", "pyproject.toml", "setup.cfg", "setup.py"], server::CLI),
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec())
    }

    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        server::toolchain(&env.toolchains)
    }

    fn locate_server(&self, env: &PackEnv, root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, root)
    }

    /// basedpyright-langserver has no version flag: its CLI answers for it.
    fn probe_server(&self, _env: &PackEnv, path: &Path, root: Option<&Path>) -> Result<String, String> {
        server::probe(path, root)
    }

    /// The root's interpreter as `python.pythonPath`.
    fn server_launch(&self, env: &PackEnv, root: &Path, _server: &FoundServer) -> Result<Launch, String> {
        Ok(Launch { settings: Some(server::root_settings(&env.home, root)), ..Launch::default() })
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, log)
    }
}
