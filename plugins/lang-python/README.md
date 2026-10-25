# Python (`lang-python`)

Everything CortexCode knows about Python, as one `lang@1` plugin (the `lang` family, see
[docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of `.py` / `.pyi`
files and `pyproject.toml` manifests, basedpyright for diagnostics, definition, references and hover,
the pytest / ruff / basedpyright verifiers, and the Python skills. A `LanguagePack` served by
`cc-lang-kit`, built like the reference plugin `lang-rust`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-python`: category `lang-python`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

Methods are called on the category: `lang-python.describe`, `lang-python.extract`, ...

- **Languages**: `python` (symbol-id prefix `py`, extensions `py`, `pyi`, `#!` interpreters `python`,
  `python3`) and `pyproject.toml` (prefix `pypi`, file name `pyproject.toml`): the memory plugin's
  names and prefixes, so the code graph's ids stay the same.
- **extract**: tree-sitter-python facts (functions, classes, methods, class attributes and `self.x`
  fields of `__init__`, module variables, with qualified names `Class.method`, signatures,
  docstrings, decorators, parameters and annotations; pytest / unittest tests by their collection
  conventions; FastAPI / Flask routes with router prefixes; imports, `__all__`, calls with argument
  text, `self` attribute reads and writes, raised exceptions, receiver-typed locals, `os.environ` /
  `os.getenv` reads, scripts run through `subprocess` / `os.system`) and pyproject.toml facts
  (project name, PEP 621 and Poetry dependencies by section, `[project.scripts]` /
  `[tool.poetry.scripts]` entry points). The extractors are the memory plugin's, ported to `lang@1`
  facts. With the extract's `root`, dependencies get their installed version and source directory
  from the project's `.venv` / `venv` / `env` site-packages.
- **Server**: basedpyright (npm `basedpyright`, pinned 1.40.2), run as
  `basedpyright-langserver --stdio`: the one installed in `$CORTEX_HOME/toolchains/basedpyright/1.40.2/`,
  else one on PATH. The language server has no version flag, so a candidate counts when the
  `basedpyright` CLI beside it answers `--version`; the pack's `probe_server` checks a
  `[plugin.config] server_path` the same way (a wrapper with no CLI beside it is accepted when it is
  executable). Settings: `basedpyright.analysis`
  `typeCheckingMode = "standard"` (a project's pyrightconfig.json / `[tool.basedpyright]` decides
  instead), `diagnosticMode = "openFilesOnly"`.
- **Interpreter**: basedpyright takes `python.pythonPath`, else the project's `.venv` (its own
  detection), else the `python` on PATH. The server of each root starts (the pack's
  `server_launch`) with `server::root_settings`: `python.pythonPath` is the session's interpreter for
  that workspace by the tools plugin's rules (the project's venv, else the automatic venv
  `$CORTEX_HOME/venvs/<hash>` once ready), unset when there is neither. The kit answers
  `workspace/configuration` with them and pushes them (`workspace/didChangeConfiguration`) after
  `initialized`. An automatic venv made after the root's server started counts from that server's
  next start (idle stop, crash, install).
- **install**: `npm install --prefix $CORTEX_HOME/toolchains/basedpyright/1.40.2 basedpyright@1.40.2`
  (never global; Node.js and npm must be present). The log says every step.
- **Toolchain for commands** (`describe`'s `path`): the installed package's `node_modules/.bin`, so
  `basedpyright` runs in commands. It never holds a Python interpreter; the tools plugin owns the
  session's Python.
- **Verifiers** (`{file}` is the file being checked; `requires`: the program the consumer checks on
  PATH, `python`, `ruff` and `basedpyright` respectively):
  - `python -m pytest -q` and `python -m pytest -q -x {file}` (test), in a project with
    `pyproject.toml`, `setup.cfg`, `pytest.ini`, `tox.ini`, `setup.py`, `conftest.py` or a `tests`
    directory;
  - `ruff check .` (lint), in a project with `ruff.toml`, `.ruff.toml` or `pyproject.toml` (ruff
    must be installed in the session's environment);
  - `basedpyright --level error` (build: the type check, errors only), in a project with
    `pyrightconfig.json`, `pyproject.toml`, `setup.cfg` or `setup.py`.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-python     # -> plugins/lang-python/target/lang-python-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-python --install`). The probe extracts a sample
through the real `extract`, reads every skill and reports whether basedpyright can run (an absent
server does not fail the probe: extraction and skills work without it).

## Limits

- The interpreter is chosen when a root's server starts; a venv created or removed later is seen
  after that server restarts.
