# Containers (`container`)

Projects whose commands run inside Docker while their files stay on the Mac. A project is one of three kinds:

- **one container** (`docker run`): the project root bind-mounted at the container's `workdir`;
- **a Docker Compose project** (`docker compose`, the v2 plugin): the compose files define the services and their bind mounts; commands and terminals run in one `service`;
- **a devcontainer with `features`**: built, created and started by the official Dev Containers CLI (`@devcontainers/cli`).

The code graph, edits, language servers and the editor work on the files as on any project, while commands (the tools plugin's `run`, terminals, benchmark verifiers such as SWE-bench Verified and Terminal-Bench) run in the container's installed environment. Contract: `container@1` (`protocol/src/contracts/container.rs`), built on the `docker` command-line tool (Docker Desktop on macOS).

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, config defaults, source location, verify commands |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-container`: category `container`, contract `container@1` |
| `verify/` | `docker.mjs`: the real frame, the real docker daemon, small public images |

## Methods

| Method | What it does |
|---|---|
| `container.attach` | Resolve the configuration, then bring the project up (below). Streams `ProgressItem`s (`phase`: `initializeCommand`, `install`, `build`, `pull`, `compose`, `devcontainer`, a lifecycle command's name; `stream`: `stdout`/`stderr`/`info`). Returns `ContainerInfo` (with `kind`, `project`, `service` and the container's bind `mounts`); publishes `container.changed` (`starting`, then `running` or `failed`). |
| `container.exec` | `docker exec -i -w <cwd> [-u user] [-e ...] <id> bash -lc <command>` (`sh -lc` when the image has no bash, detected once per container), in the container commands run in (for Compose: `service`'s). Streams `ExecItem`s (`stdout`/`stderr`), returns `ExecResult` (exit code, interleaved output, duration, `timed_out`, `cancelled`). |
| `container.status` | The root's container as Docker reports it now (`null` when there is none). |
| `container.list` | Every container commands run in that this plugin manages (label `dev.cortexcode.managed=1`), plus failed attaches. |
| `container.detach` | `docker rm -f` the container, or `docker compose -p <project> down` the Compose project (by name, so it works after the compose files changed; also a project whose service container is already gone). Named volumes are kept unless `remove_volumes: true` (`down -v`, `rm -v`). The project goes back to running on the Mac. |
| `container.shell_command` | The argv of an interactive login shell for terminals: `<docker> exec -it -w <workdir> [-u user] [-e ...] <id> bash -l` (argv[0] is the docker CLI's absolute path, since a terminal may not have Docker Desktop's directory on its PATH). |
| `container.ps` | The project's containers (every Compose service), main one first: id, name, service, image, Docker state, health, exit code, start time, published ports. |
| `container.ports` | Ports the project's containers publish on the Mac (`NetworkSettings.Ports`): container port, protocol, host ip and port, service. |
| `container.logs` | `docker logs --tail <n> [--since] [--timestamps]` of the main container, or of every container of `service`; streams `LogItem`s, returns each container's output (the latest `max_output_bytes`). `tail` defaults to 1000 lines. |
| `container.images` | `docker image ls` (optionally `--filter reference=<pattern>`): id, repository, tag, size, created. |
| `container.build` | `docker build` of a Dockerfile (default `<root>/Dockerfile`, context the root, tag `cortex-build-<hash of root>:latest`; `target`, `build_args`, `platform`, `no_cache`, `pull`), progress streamed as items; returns the image's tag, id and platform. |
| `container.pull` | `docker pull [--platform]`, progress streamed as items; returns tag, id, platform. |

No method removes anything outside a project (no prune, no image removal).

## Configuration sources

In order: attach parameters that name `image`, `dockerfile` or `compose` (source `attach`: they are the whole configuration); else `.cortex/container.toml`; else `.devcontainer/devcontainer.json` (or `.devcontainer.json`). With a file, the attach parameters `workdir`, `platform`, `env`, `network`, `seed_from_image` override or extend it where they apply (one container: all; Compose: `workdir`, `env`; features: none); a parameter that does not apply is an error naming it. A Dockerfile given as an attach parameter builds with the project root as context; an image built without a name is tagged `cortex-build-<hash of root>:latest`.

Compose attach parameters: `compose` (files, relative to the root or absolute), `service`, `profiles`, `env_files`, `run_services`, `compose_env` (variables for `${VAR}` interpolation in the compose files, e.g. Terminal-Bench's `T_BENCH_*`), `env` (added to `service`'s environment), `workdir`.

### `.cortex/container.toml`

Every key is optional except that `image`, `dockerfile` or `compose` is required; unknown keys are an error. Relative paths are relative to the project root.

```toml
image = "python:3.12-slim"     # image to run; with `dockerfile`, the tag the built image gets
dockerfile = "Dockerfile"      # build this first (docker build --platform ... -t <image> -f ...)
context = "."                  # build context (default: the project root)
target = "dev"                 # build stage
build_args = { PY = "3.12" }   # --build-arg
workdir = "/workspace"         # where the project root is mounted (default /workspace)
platform = "linux/amd64"       # linux/amd64 | linux/arm64; default: the image's own
network = true                 # false: --network none
seed_from_image = false        # copy the image's workdir content into the (empty) root first
ports = ["8000:8000"]          # published ports (docker run -p)
user = "dev"                   # user commands and terminals run as (default: the image's)
override_command = true        # false: keep the image's own long-running command (see below)

[env]                          # container environment, seen by every command
RUST_BACKTRACE = "1"
```

A Docker Compose project:

```toml
compose = ["compose.yaml", "compose.dev.yaml"]   # one file (a string) or several (-f, in order)
service = "app"                # required: where commands and terminals run
profiles = ["dev"]             # --profile
env_file = ".env.dev"          # one file or several (--env-file)
run_services = ["app", "db"]   # services to start (default: all of the enabled profiles); `service` always
workdir = "/app"               # default: where the root is mounted in `service`, else its working_dir, else /
user = "node"                  # user commands run as (docker exec -u)
override_command = false       # true: replace `service`'s command with the keep-alive loop

[env]                          # added to `service`'s environment
DEBUG = "1"

[compose_env]                  # ${VAR} interpolation in the compose files
TAG = "v2"
```

`image`, `dockerfile`, `context`, `target`, `build_args`, `platform`, `network`, `seed_from_image` and `ports` are errors with `compose`: the compose files say those things.

### `devcontainer.json`

JSON with comments and trailing commas. Supported:

- image/Dockerfile configurations: `image`; `build.dockerfile`, `build.context` (relative to the devcontainer.json folder, default `.`), `build.args`, `build.target`, `build.options`, `build.cacheFrom` (and the older top-level `dockerFile`/`context`); `runArgs`; `appPort` (a number is published as `127.0.0.1:<n>:<n>`, like the Dev Containers CLI; a string is a `docker run -p` value); `workspaceMount` (replaces the root's bind mount; needs `workspaceFolder`, as the spec says);
- Compose configurations: `dockerComposeFile` (one or several, relative to the devcontainer.json folder), `service`, `runServices`; `workspaceFolder` defaults to `/` and `overrideCommand` to false (devcontainer spec);
- both: `workspaceFolder` (default `/workspaces/<root folder name>`); `containerEnv`; `remoteEnv` (per command, `${containerEnv:VAR}` resolved from the container's environment); `remoteUser` (defaults to `containerUser`); `containerUser`; `overrideCommand`; `mounts`; `privileged`; `capAdd`; `securityOpt`; `init`; `forwardPorts`; `features` (see below); `initializeCommand` (on the Mac, in the root, before every attach); the lifecycle commands `onCreateCommand`, `updateContentCommand`, `postCreateCommand` (once, after creation), `postStartCommand` (every start), `postAttachCommand` (every attach), each a string (`/bin/sh -c`), an argv array or an object of commands run in parallel, as `remoteUser`, in the workspace folder; a failing one fails the attach and removes the container (for Compose: the service's container, so the next attach creates it again).
- Variables: `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`, `${containerWorkspaceFolder}`, `${containerWorkspaceFolderBasename}`, `${localEnv:VAR[:default]}`, `${containerEnv:VAR[:default]}` (remoteEnv only).

`forwardPorts`: a number, or `"localhost:<n>"`, is published for the container commands run in; with Compose, `"<service>:<n>"` is published for that service. Each is published on 127.0.0.1 with the same port when it is free on the Mac, else on a port Docker picks (VS Code likewise forwards to another local port when one is taken); a port the project already publishes keeps its binding. `container.ports` reports the actual ports.

Editor-only keys (`name`, `customizations`, `portsAttributes`, `otherPortsAttributes`, `shutdownAction`, `hostRequirements`, `userEnvProbe`, `updateRemoteUserUID`, `waitFor`) are ignored. Unknown keys, and keys that do not apply to the kind of configuration (`runArgs`, `appPort`, `workspaceMount`, `image`, `build` with `dockerComposeFile`; `service`, `runServices` without it), are errors that name them, never silently dropped.

## How it works

- **Identity and restarts.** Every container commands run in is labelled `dev.cortexcode.root`, `.kind`, `.workdir`, `.image`, `.platform`, `.source`, `.config` (digest of the configuration, for one container also of the exact image id) and `.exec` (exec user and remoteEnv), plus `.project`/`.service` (Compose) or `.config_file` (features). Everything is recoverable from Docker, so after a plugin restart or hot swap the container is found again by its root label; nothing is exported. The bind mounts come from `docker inspect` each time (a source under Docker Desktop's `/host_mnt` is the Mac path without it).
- **One container** is named `cortex-<16 hex of sha256(root)>`. Attach reuses a running container with the same digest, starts a stopped one, and replaces one whose configuration or image changed (also a container of another kind: a Compose project of the root is brought `down`, volumes kept).
- **Compose.** The project is `cortex-<16 hex of sha256(root)>` (stable per root). Attach checks `docker compose version`, validates the files and names with `docker compose config --format json` (an unknown `service`/`runServices`/forwarded service is an error listing the defined ones), writes an override file (in the plugin's data directory) that gives `service` this plugin's labels and the devcontainer settings (`containerEnv` as `environment`, `containerUser` as `user`, `mounts` as long-syntax volumes, `privileged`, `init`, `cap_add`, `security_opt`, the keep-alive entrypoint when `overrideCommand` is true) and `ports` for `forwardPorts`, every value escaped for Compose interpolation (`$` → `$$`), then runs `docker compose -p <project> -f <files...> -f <override> [--profile ...] [--env-file ...] --ansi never up -d --build --wait --remove-orphans [run_services]` in the root with `compose_env`: Compose builds what has a build section, pulls what is missing, recreates what changed (its own config hash) and waits until every started service runs and its healthcheck passes. The lifecycle commands run in `service`'s container: create commands when its id is new, start commands when its start time changed, attach commands always. Compose's own variables (`COMPOSE_FILE`, `COMPOSE_PROJECT_NAME`, ...) are cleared from the plugin's environment for every docker call.
- **Paths.** Host paths under the root (in the caller's spelling or the canonical one, `/tmp` vs `/private/tmp`) map through the container's bind mount with the longest source that contains them (the root itself, a directory inside it such as Compose's `./data:/srv/data`, or a parent such as a `workspaceMount` of `..`); a root path that no bind mount brings into the container is an error naming the mounts. Other absolute paths are container paths; relative paths are relative to `workdir`.
- **Features (the Dev Containers CLI).** A devcontainer.json with non-empty `features` is handed to the official CLI rather than reimplemented: installing a feature means resolving OCI artifacts on registries (token flow, credential helpers, semver tags), ordering by `dependsOn`/`installsAfter`/`overrideFeatureInstallOrder`, generating a layered build whose install scripts see their options and the remote/container users and homes, and merging every feature's metadata (`containerEnv`, `mounts`, `privileged`, `capAdd`, `securityOpt`, `init`, `entrypoint`, lifecycle commands that run before the configuration's). The CLI is the spec's reference implementation (VS Code, Codespaces) and follows the spec as it evolves; a second implementation would drift. So `devcontainer up --workspace-folder <root> --config <file> --docker-path <docker> --mount-workspace-git-root=false --id-label <each label>` builds, creates or reuses (by the labels) and starts the container and runs `initializeCommand` and every lifecycle command (the features' too); for a Compose-based configuration `COMPOSE_PROJECT_NAME` is `cortex-<hash>`. `forwardPorts`, which the CLI does not publish, go into an `--override-config` (as `appPort` entries, or for Compose a compose file with `ports` appended to `dockerComposeFile`; relative paths made absolute). `devcontainer read-configuration --include-merged-configuration --container-id <id>` gives the merged `remoteUser` and `remoteEnv` (asked again after a plugin restart). Commands and terminals then use `docker exec` like every other kind, so timeouts, cancellation and streaming behave the same. The CLI reuses a container whose labels match, so the digest label includes the Dockerfile's content (an edited Dockerfile or devcontainer.json replaces the container; a feature tag that moved upstream is picked up by detach + attach). The CLI is installed on first use with `npm install --prefix` into `$CORTEX_HOME/toolchains/devcontainers-cli/<version>` (staged in a sibling directory and renamed into place; never global), run as `<node> <its bin script>`; or plugin config `devcontainer_cli` names an existing executable. It needs Node.js: `node` (config, else PATH, else `/opt/homebrew/bin`, `/usr/local/bin`) and npm next to it or on PATH.
- **Images whose command exits.** By default the image's command is replaced by a keep-alive loop (`--entrypoint /bin/sh ... -c "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done"`): benchmark images usually have `CMD ["/bin/bash"]`, which exits at once without a terminal. `override_command = false` (`overrideCommand: false`) keeps the image's own command for images that run a service; if that command ends right away the attach fails with that explanation.
- **Seeding** (`seed_from_image`, one container only): `docker create` + `docker cp <tmp>:<workdir>/. <root>` into an empty root (created when missing), then `docker rm`. The plugin records which image seeded the root (in its data directory, not in the project), so attaching again after a detach keeps the files, edits included. A non-empty root that was not seeded from that image is refused instead of overwritten.
- **Images and platforms.** A Dockerfile is built on every attach (Docker's cache makes an unchanged build quick); a missing image, or one for another platform than requested, is pulled with `--platform`. Builds print plain progress (`BUILDKIT_PROGRESS=plain`).
- **Timeout and cancellation.** Killing the `docker exec` client does not stop the command inside the container. Every exec carries `CORTEX_EXEC_ID=<uuid>` in its environment; on timeout, cancellation or plugin shutdown the plugin runs a small script as root inside the container that finds the processes with that marker and all their descendants through `/proc`, stops them (so they cannot fork), then kills them; it waits for the client to end (repeating the kill up to three times) and finally kills the client. The output written before the kill is in the result. A cancelled attach, build, pull or logs call kills its docker/compose/CLI process.
- **Output.** stdout and stderr are read concurrently; every chunk is an item (UTF-8 sequences are never split across items) and is appended to the result's output in the order read. The result keeps the latest `max_output_bytes` (default 8 MiB, far below the 64 MiB protocol frame), prefixed by a line saying how many earlier bytes were omitted; the items carry everything.
- **Docker, Compose or node missing.** The CLI is looked up as the configured `docker`, then on `PATH`, then at Docker Desktop's and Homebrew's locations (an app started from the Dock has a minimal PATH); its directory is added to the PATH of docker processes (credential helpers live there). Without the CLI, or with the daemon down, attach/exec/detach fail with `unavailable` and a message saying which; without the Compose plugin a Compose attach fails with `unavailable`; without node a features attach does. `status`/`list` still answer (remembered containers are reported `stopped` with the reason). The probe's `docker`, `compose` and `devcontainer_cli` checks pass with a detail starting `problem:` when something is missing (the plugin stays useful without Docker); `devcontainer_cli` otherwise says whether the CLI is ready or where it will be installed. A probe installs nothing.

Plugin config (`[plugin.config]`): `docker` (CLI path), `max_output_bytes`, `stop_wait_ms` (wait for the command to end after each kill, default 3000), `devcontainer_cli_version` (default `0.75.0`), `devcontainer_cli` (an existing `devcontainer` executable), `node`.

Notes: `bash -l` reads the image's login profile; on Debian-based images `/etc/profile` resets `PATH` (images that only extend `PATH` through `ENV` lose those entries in login shells unless their profile adds them back; SWE-bench images activate their environment from `~/.bashrc`, which Debian's `~/.profile` sources). Bind mounts need their sources under a directory Docker Desktop shares (`/Users`, `/Volumes`, `/private`, `/tmp` and `/var/folders` by default). `platform` is not tracked for Compose services (`null`; the compose file decides it).

## Build and verify

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
node verify/docker.mjs        # needs Docker Desktop running, network and node; pulls alpine:3.20, debian:12-slim (also linux/amd64), postgres:16-alpine; installs @devcontainers/cli into the frame's temporary CORTEX_HOME
cortex-pack build plugins/container
```

The unit and integration tests need no Docker: `backend/tests/fake_docker.sh` is a fake docker CLI put on a temporary PATH (it runs exec'd commands on the host, so streaming, timeouts and kills are real processes; its `compose` subcommand creates the services of a fixture, reading labels and ports from the plugin's JSON override with `plutil`), `fake_devcontainer.sh` is a fake Dev Containers CLI that creates and reuses containers through the fake docker by their id labels, and `fake_npm.sh` (with a fake `node`) "installs" it into the toolchains directory. The in-container kill script is run under dash against a fabricated `/proc`. The frame runs it once installed: `cortex-pack build plugins/container --install`.
