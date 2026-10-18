Docker here: get it right, build and lint it, Dockerfiles that run, common errors and fixes.

## 1. Get it right

1. **Pin down what the image must do.** Read the request, then the Dockerfile, compose file, `.dockerignore`, and how the app starts (README, package scripts, `CMD`).
   - Note: base image and tag, needed files, listening port, command, user, environment.
   - The request and the project decide, never a name alone: read what stage `prod` or service `api` really runs.
   - The request and the project disagree: say so and ask, instead of silently picking one.
   - Before editing, note the request's examples: they are the check, their values copied, never computed.
   - Unfamiliar base image, tool or setting: look it up (the research plugin, the image's docs, how others solved it):
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
2. **Write whole stages, in slices.**
   - First the image builds and the app starts.
   - Then each requirement (user, port, files, environment), build and run after each.
   - Change only the lines the task needs; a stage that builds and runs stays exactly as it is, unless the user asks to change it. Never leave an old `RUN`, `COPY` or `CMD` behind when you replace one (a second `CMD` silently wins).
3. **Build after every change.** Read the failing step (section 4).
4. **Run what you built.** Compare output, user, files and port with the request.

```bash
docker build -t app:dev .
docker run --rm app:dev sh -c 'id; ls -la /app'
docker run -d --name app-test -p 8000:8000 app:dev
for i in $(seq 30); do curl -fsS 127.0.0.1:8000/ && break; sleep 1; done; docker logs --tail 50 app-test
docker rm -f app-test
```

5. **Before you finish**, check each requirement again (tag, user, port, entrypoint, environment, paths). Remove your containers.


## 2. Build, run, check

```bash
hadolint Dockerfile                 # lint, when installed; exit 1 on any finding
docker build --check .              # BuildKit's lint rules, no image built
docker build -t app:dev .           # -f api.Dockerfile, --target build for others
docker compose config --quiet       # compose syntax and ${VAR} values; exit 0 = valid
docker compose up -d --build --wait # start all, wait for healthchecks
docker compose ps -a; docker compose logs --no-color --tail=200 web
docker compose run --rm -T web pytest -q    # a one-off command in a service
```

- `run` has no TTY and empty stdin: never `-it`; `-T` with `docker compose exec`/`run`. Interactive shells: `terminal`.
- Project runs in a container: `run` is already inside it, `docker` is absent. Run the commands directly.
- `Cannot connect to the Docker daemon`: `open -a Docker`, then `for i in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 2; done`.
- Pulling a base image needs the network; `docker image ls` shows what is already here.
- This Mac is arm64. `no matching manifest for linux/arm64/v8`: multi-arch tag or `--platform linux/amd64`.
- `docker compose down -v` deletes volumes (databases). Never `docker system prune`.

**Checking your work.** Run the project's own tests inside the image (`docker compose run --rm -T web pytest -q`, or the project's test command), then check what the change is about: the app starts, answers on its port, runs as its user, has its files.

```bash
docker build -q -t app:check . && docker run --rm app:check sh -c 'id; ls -la /app'
docker run -d --name app-check -p 8000:8000 app:check
for i in $(seq 30); do curl -fsS 127.0.0.1:8000/health && break; sleep 1; done; docker logs --tail 50 app-check
docker rm -f app-check
```

- Expected values come from the request or the project (README, compose file, docs), never from what the container printed.
- After any fix, run every check again, passing ones too.
- Remove the `app:check` image and your containers when the work is done.

## 3. Dockerfiles that run

Install dependencies before copying the source:

```dockerfile
COPY . .                                   # bad: every source edit reruns npm ci
RUN npm ci
COPY package.json package-lock.json ./     # good: manifests, install, then source
RUN npm ci
COPY . .
```

- Same for `requirements.txt`, `go.mod`+`go.sum`, `Cargo.toml`+`Cargo.lock`.
- `.dockerignore` holds at least `.git`, `node_modules`, `.venv`, `target`, `.env`.

Update and install in one layer:

```dockerfile
RUN apt-get update                         # bad: cached alone, later installs use stale lists
RUN apt-get install -y curl
RUN apt-get update && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*         # good; Alpine: apk add --no-cache curl
```

Pin the base, run as a user:

```dockerfile
FROM python                                # bad: latest, a different image next month
FROM python:3.12-slim                      # good: the tag the project or the request names
RUN useradd --system --uid 10001 --no-log-init app   # before USER: create and chown what it writes
COPY --chown=app:app . /app
USER app
```

Commands and processes:

```dockerfile
RUN cd /app && make                        # cd lasts one RUN only
WORKDIR /app                               # good: absolute, for every later instruction
CMD npm start                              # bad: sh is PID 1, the app never gets SIGTERM
CMD ["node", "server.js"]                  # good: exec form
CMD ["node", "server.js", "--port", "$PORT"]            # bad: exec form expands no variables
CMD ["sh", "-c", "exec node server.js --port \"$PORT\""] # good
```

- Listen on `0.0.0.0` in the container; `127.0.0.1` is unreachable through `-p`.
- `COPY` paths are relative to the build context (`.` of `docker build`), not the Dockerfile.
- An `ARG` before `FROM` is seen only by `FROM` lines. Repeat `ARG NAME` in the stage.
- Secrets never in `ENV` or `ARG` (they stay in the image): `RUN --mount=type=secret,id=token ...`.
- A piped `RUN` counts only the last command: `SHELL ["/bin/bash", "-o", "pipefail", "-c"]` before it.
- An entrypoint script ends with `exec "$@"`, has LF endings and the execute bit (`COPY --chmod=755`).
- Compose: services reach each other by service name and container port (`db:5432`), not `localhost`.

## 4. Errors

Read the failing step (`#N [stage k/n] RUN ...`, then `#N ERROR`), not the last `failed to solve` line. Full log: `artifact://<id>`.

| Error | Fix |
|---|---|
| `failed to compute cache key: "/x": not found` | COPY source outside the context, excluded by `.dockerignore`, or misspelled |
| `exit code: 100`, `Unable to locate package` | `apt-get update` not in the same `RUN`, or a wrong package name |
| `exit code: 127` | program missing in that image: install it in that `RUN`, or `sh` for `bash` |
| `exec format error`, `no such file or directory` for an existing file | CRLF in the script's shebang, wrong CPU architecture, or a glibc binary on Alpine |
| container exits 0 at once | the server runs in the background: run it in the foreground |
| `connection refused` through a published port | the app listens on `127.0.0.1`: bind `0.0.0.0` |

Lint codes:
- DL3006, DL3007: pin the base tag. DL3002: final `USER` not root.
- DL3003: `WORKDIR`, not `cd`. DL3000, `WorkdirRelativePath`: absolute `WORKDIR`.
- DL3009, DL3015: `--no-install-recommends` and `rm -rf /var/lib/apt/lists/*` in the install's `RUN`.
- DL3019: `apk add --no-cache`. DL3042: `pip install --no-cache-dir`.
- DL3008, DL3018: pin package versions only where the project already does.
- DL3020: `COPY`, not `ADD`. DL3025, `JSONArgsRecommended`: exec form. DL4006: `SHELL` with `pipefail`.
- `UndefinedVar`: `ARG` in that stage. `SecretsUsedInArgOrEnv`: secret mount. `CopyIgnoredFile`: source is in `.dockerignore`.
