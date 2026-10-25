# Docker (`lang-docker`)

Everything CortexCode knows about Dockerfiles and Compose files, as one `lang@1` plugin (the `lang`
family, see [docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of
Dockerfiles / Containerfiles and Compose files, dockerfile-language-server and
compose-language-service for diagnostics, definition, references and hover, the docker and hadolint
verifiers, the docker CLI on commands' PATH, and the Docker skills. It is a `LanguagePack` served by
`cc-lang-kit`, built like the reference plugin `lang-rust`. (Running project commands inside
containers is the separate `container` plugin; this one is the language side.)

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-docker`: category `lang-docker`, contract `lang@1`; `cc-lang-docker lsp-router` is the language-server router |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

Methods are called on the category: `lang-docker.describe`, `lang-docker.extract`, ...

- **Languages**: `dockerfile` (symbol-id prefix `docker`; file names `Dockerfile`, `Containerfile`,
  `dockerfile`, `containerfile`; patterns `Dockerfile.*`, `Containerfile.*` (and lower case),
  `*.Dockerfile`, `*.Containerfile` (and lower case); extensions `Dockerfile`, `dockerfile`,
  `Containerfile`, `containerfile` too, for consumers that read extensions only) and `compose`
  (prefix `compose`; file names `compose.yaml`, `compose.yml`, `docker-compose.yaml`,
  `docker-compose.yml`; patterns `compose.*.yaml`, `compose.*.yml`, `docker-compose.*.yaml`,
  `docker-compose.*.yml`). A per-Dockerfile ignore file named `Dockerfile.dockerignore` matches
  `Dockerfile.*` too (it parses with errors; nothing else claims it).
- **extract** (`src/dockerfile.rs`, tree-sitter-containerfile 0.9.2; `src/compose.rs`,
  tree-sitter-yaml 0.7.2; both on the kit's tree-sitter 0.25):
  - Dockerfile: every `FROM` is a `class` symbol (the `AS` name, else `stage-<n>`; base in
    `extends` / `type_text` with global `ARG` defaults substituted; `--platform` in `decorators`;
    the last stage `exported`); `ARG` / `ENV` pairs are `const` symbols with values (global or
    `<stage>.<NAME>`); `EXPOSE` ports are `field` symbols. Base images and `COPY --from=<image>` are
    `docker` dependencies (`dependencies` for the final stage's base, `build` otherwise; `scratch`
    and unresolvable `${VAR}` images are left out); `FROM <stage>` / `COPY --from=<stage|index>` are
    `Instantiate` references; `$VAR` / `${VAR}` in instruction arguments are `Read` references;
    `COPY` / `ADD` sources are imports (module = the source as written, local = the destination);
    `CMD` / `ENTRYPOINT` / `HEALTHCHECK` are commands (`<stage>:cmd`, ...). Script files that `RUN`,
    `CMD`, `ENTRYPOINT` and `HEALTHCHECK` name become runs (and the commands' `runs_files`) when the
    container path maps back through the stage's `COPY` / `ADD` (inherited by `FROM <stage>`, with
    `WORKDIR`) or a `RUN --mount=type=bind` to a file that exists under the repository root (the
    build context is taken to be the Dockerfile's directory, then the root); nothing is claimed
    without a root.
  - Compose: `name` is the package; services are `class` symbols (image in `type_text`,
    `extends.service` in `extends`, profiles in `decorators`) with `ports` / `expose` `field` and
    `environment` `const` children; top-level `volumes`, `networks`, `secrets`, `configs` are
    `variable` symbols. `image` (of a service that does not build) is a `docker` dependency; `build`
    is a `docker-build` dependency (name: the context, spec: the Dockerfile path) and a run of the
    Dockerfile when it exists. `depends_on` (condition as argument), `links`, `volumes_from`,
    `network_mode: service:x` and same-file `extends` are `Instantiate` references; named volumes,
    networks, secrets and configs are `Read` references; `${VAR}` interpolations are `ReadConfig`
    references. Bind mounts, `env_file`, `include` and `extends.file` are imports; `command`,
    `entrypoint` and `healthcheck.test` are commands whose script files map back through bind mounts.
    Anchors, aliases and `<<` merge keys are resolved.
- **Server**: dockerfile-language-server-nodejs 0.15.0 (`docker-langserver`) for Dockerfiles and
  @microsoft/compose-language-service 1.0.0 (`docker-compose-langserver`) for Compose files, both
  run with Node.js (22 or newer: the compose server's requirement) over stdio. Documents are opened
  by the file names and patterns above (`ServerSpec::file_names`), so other YAML never reaches the
  servers. The kit drives one server per workspace, so the plugin's own executable runs as
  `cc-lang-docker lsp-router`
  (`src/router.rs`): it starts both, answers `initialize` with their merged capabilities, routes each
  document by file name (with the server's `languageId`), keeps request ids, progress tokens and
  server-to-client requests apart, stamps unversioned diagnostics with the version last sent,
  answers Dockerfile references from document highlights (the declaration excluded), and answers
  Compose definition and references itself from the Compose structure (services, volumes,
  networks, secrets, configs); neither server provides those.
- **install**: `npm install --prefix` of both pinned packages into
  `$CORTEX_HOME/toolchains/{dockerfile-language-server,compose-language-service}/<version>/`, then
  hadolint 2.14.0's static macOS binary from its GitHub release (SHA-256 pinned per architecture)
  into `toolchains/hadolint/2.14.0/`. hadolint is optional: a failed download is logged and does not
  fail the install. Homebrew and global npm are never touched.
- **Verifiers** (all `lint`, declared whether or not the tools are installed; the consumer checks
  `requires` on commands' PATH, which the toolchain below extends): `docker compose config --quiet`
  (with a standard compose file) and `docker build --check .` (with a `Dockerfile`), requiring
  `docker`; `hadolint Dockerfile`, requiring `hadolint`.
- **Toolchain** (`Description.path` / `env`): hadolint's directory when installed; the docker CLI's
  real directory (symlinks resolved, e.g. Docker Desktop's `Contents/Resources/bin`) when docker is
  not on PATH but installed by Docker Desktop, OrbStack, Rancher Desktop, Homebrew or in
  `~/.docker/bin`; `BUILDKIT_PROGRESS=plain`, `COMPOSE_PROGRESS=plain`, `COMPOSE_ANSI=never`,
  `COMPOSE_MENU=false`, `DOCKER_CLI_HINTS=false` so command output is plain text.

Not used: Docker's own `docker-language-server` (Go, 0.20.1). Its Dockerfile features are lint
through `docker buildx build --call=check` (needs a running daemon) and Docker Scout hovers
(network); for Compose it reports YAML syntax errors, definitions and highlights, which the router
covers from the Compose structure. It also sends telemetry by default (its first event is queued
before the `telemetry: "off"` initialization option is read; only the
`DOCKER_LANGUAGE_SERVER_TELEMETRY=false` environment variable stops publishing), and it is a 40 MB
download per architecture.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-docker     # -> plugins/lang-docker/target/lang-docker-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-docker --install`). The probe extracts a
sample Dockerfile through the real `extract`, reads every skill and reports whether the servers can
run (absent servers do not fail the probe).

## Why the router stays

The kit runs one server process per workspace root. Running the two servers as kit-level servers
would need more than starting two processes: per-document server choice, each server's own sync
capabilities, open-document set, progress and readiness, a merged settle condition, and crash /
restart handling per server, plus pack hooks that answer requests neither server can (Compose
definition and references from the Compose structure, Dockerfile references from document
highlights) and that stamp unversioned publications. The router does all of that in one place
(`src/router.rs`, tested on its own); moving it into the kit would make every other pack's server
path more complex for one plugin's benefit, so it stays.
