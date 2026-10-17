"""CortexCode as a custom Harbor agent for Terminal-Bench 2.0, and the Harbor environment it needs.

Harbor (the official TB 2.0 harness) runs a trial as: start the task environment, run the agent
with the task instruction, run the task's tests in the same environment (/tests/test.sh writes
/logs/verifier/reward.txt), record result.json. CortexCode reaches a task container only through
its container plugin (`container.attach`, protocol/src/contracts/container.rs), which starts and
owns the container. So the task container has to be the one CortexCode attached, and Harbor's
verifier has to run in it. That is what these two classes do:

  CortexContainerEnvironment  a Harbor environment (`--env cceval_tb:CortexContainerEnvironment`)
      start()  pulls the task's prebuilt image (or builds environment/Dockerfile with
               --force-build), reads the image's WORKDIR, and asks the eval harness to
               `container.attach` it {image, workdir, seed_from_image: true}: the workdir's content
               is copied to the host workspace and bind-mounted back; everything else in the
               container (installed packages, /etc, /root, /protected, ...) stays in the container
               filesystem, where both the agent's commands and the tests run. The task's cpu and
               memory limits are applied to that container with `docker update`, as Harbor's
               docker environment applies them through compose.
      exec / upload / download  `docker exec` / `docker cp` on the attached container (the
               plugin runs Docker; ContainerInfo.id is the Docker container id). Harbor's verifier
               uploads /tests, runs test.sh and downloads /logs/verifier through these.
      stop()   asks the harness to `container.detach` it.
  CortexAgent  a Harbor agent (`--agent cceval_tb:CortexAgent`): run() hands the instruction to the
      eval harness, which sends it to CortexCode (chatflow.turn) and answers when the turn is over.
      Harbor's own agent timeout (task.toml [agent].timeout_sec) cancels run(); the cancellation is
      forwarded so CortexCode stops before the verifier starts.

The eval harness (eval/suites/terminalbench/suite.mjs) is on the other end of a JSON-lines channel:
requests on fd 3, replies on fd 4, matched by id.
"""

from __future__ import annotations

import asyncio
import json
import os
import shlex
import shutil
import threading
from pathlib import Path

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment, ExecResult
from harbor.environments.capabilities import EnvironmentCapabilities, EnvironmentResourceCapabilities
from harbor.environments.definition import (
    require_agent_environment_definition,
    should_use_prebuilt_docker_image,
)
from harbor.models.agent.context import AgentContext
from harbor.models.task.config import EnvironmentConfig
from harbor.models.trial.config import ResourceMode
from harbor.models.trial.paths import EnvironmentPaths, TrialPaths


class _Channel:
    """JSON-lines requests to the eval harness (fd 3) with replies matched by id (fd 4)."""

    def __init__(self, out_fd: int = 3, in_fd: int = 4):
        self._out = os.fdopen(out_fd, "w", buffering=1)
        self._in = os.fdopen(in_fd, "r")
        self._lock = threading.Lock()
        self._pending: dict[int, tuple[asyncio.AbstractEventLoop, asyncio.Future]] = {}
        self._next = 1
        threading.Thread(target=self._read, name="cceval-channel", daemon=True).start()

    def _read(self) -> None:
        for line in self._in:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            with self._lock:
                waiter = self._pending.pop(msg.get("id"), None)
            if waiter is not None:
                loop, fut = waiter
                loop.call_soon_threadsafe(lambda f=fut, m=msg: f.done() or f.set_result(m))
        with self._lock:
            waiters, self._pending = list(self._pending.values()), {}
        for loop, fut in waiters:
            loop.call_soon_threadsafe(lambda f=fut: f.done() or f.set_exception(RuntimeError("eval harness channel closed")))

    async def request(self, event: str, **data) -> dict:
        loop = asyncio.get_running_loop()
        fut = loop.create_future()
        with self._lock:
            rid = self._next
            self._next += 1
            self._pending[rid] = (loop, fut)
            self._out.write(json.dumps({"id": rid, "event": event, **data}) + "\n")
        msg = await fut
        if not msg.get("ok"):
            raise RuntimeError(f"eval harness: {event} failed: {msg.get('error')}")
        return msg


_CHANNEL: _Channel | None = None


def channel() -> _Channel:
    global _CHANNEL
    if _CHANNEL is None:
        if os.environ.get("CCEVAL_TB_CHANNEL") != "fd3-fd4":
            raise RuntimeError("cceval_tb runs only under the eval harness (eval/run.mjs --suite terminalbench)")
        _CHANNEL = _Channel()
    return _CHANNEL


class CortexContainerEnvironment(BaseEnvironment):
    """Harbor environment backed by the container CortexCode's container plugin attached."""

    @staticmethod
    def type() -> str:
        return "cceval-cortex-container"

    @classmethod
    def preflight(cls) -> None:
        if not shutil.which("docker"):
            raise SystemExit("the docker CLI is required")
        channel()

    def __init__(self, environment_dir: Path, environment_name: str, session_id: str, trial_paths: TrialPaths,
                 task_env_config: EnvironmentConfig, *args, **kwargs):
        super().__init__(environment_dir=environment_dir, environment_name=environment_name, session_id=session_id,
                         trial_paths=trial_paths, task_env_config=task_env_config, *args, **kwargs)
        self._image_name = "hb__" + "".join(c if c.isalnum() or c in "._-" else "_" for c in environment_name.lower())
        self._container_id: str | None = None
        self._use_prebuilt = False

    @property
    def capabilities(self) -> EnvironmentCapabilities:
        # Log directories are not bind-mounted: the plugin mounts only the workdir. Harbor then
        # copies /logs/agent and /logs/verifier out after each phase.
        return EnvironmentCapabilities(mounted=False)

    @classmethod
    def resource_capabilities(cls) -> EnvironmentResourceCapabilities:
        return EnvironmentResourceCapabilities(cpu_limit=True, memory_limit=True)

    def _validate_definition(self):
        require_agent_environment_definition(self.environment_dir, docker_image=self.task_env_config.docker_image)

    async def _docker(self, args: list[str], *, check: bool = True, timeout_sec: float | None = None) -> ExecResult:
        proc = await asyncio.create_subprocess_exec(
            "docker", *args, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        try:
            out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout_sec) if timeout_sec else await proc.communicate()
        except asyncio.TimeoutError:
            proc.kill()
            await proc.communicate()
            raise RuntimeError(f"Command timed out after {timeout_sec} seconds") from None
        except BaseException:
            if proc.returncode is None:
                proc.kill()
                await proc.communicate()
            raise
        res = ExecResult(stdout=out.decode(errors="replace") if out else None,
                         stderr=err.decode(errors="replace") if err else None, return_code=proc.returncode or 0)
        if check and res.return_code != 0:
            raise RuntimeError(f"docker {' '.join(args[:3])} failed ({res.return_code}): {res.stderr or res.stdout}")
        return res

    async def start(self, force_build: bool):
        self._use_prebuilt = should_use_prebuilt_docker_image(
            self.environment_dir, docker_image=self.task_env_config.docker_image, force_build=force_build,
        )
        if self._use_prebuilt:
            image = self.task_env_config.docker_image
            if (await self._docker(["image", "inspect", image], check=False)).return_code != 0:
                await self._docker(["pull", image])
        else:
            image = self._image_name
            dockerfile = (self.environment_dir / "Dockerfile").resolve()
            await self._docker(["build", "-t", image, "-f", str(dockerfile), str(self.environment_dir.resolve())])
        info = json.loads((await self._docker(["image", "inspect", image])).stdout)[0]
        workdir = self.task_env_config.workdir or info.get("Config", {}).get("WorkingDir") or ""
        if workdir in ("", "/"):
            raise RuntimeError(f"{self.environment_name}: the image has no WORKDIR; the container plugin mounts the project at the workdir")
        arch = info.get("Architecture") or "amd64"
        platform = "linux/arm64" if arch in ("arm64", "aarch64") else "linux/amd64"
        reply = await channel().request("attach", image=image, workdir=workdir, platform=platform,
                                        env=[[k, v] for k, v in self._startup_env().items()],
                                        prebuilt=self._use_prebuilt)
        self._container_id = reply["container"]["id"]
        self._workdir = workdir
        update = []
        cpus = self._resource_limit_value("cpu", auto_mode=ResourceMode.LIMIT)
        memory_mb = self._resource_limit_value("memory", auto_mode=ResourceMode.LIMIT)
        if cpus:
            update += ["--cpus", str(cpus)]
        if memory_mb:
            update += ["--memory", f"{memory_mb}m", "--memory-swap", f"{memory_mb}m"]
        if update:
            await self._docker(["update", *update, self._container_id])
        paths = EnvironmentPaths.for_os(self.os)
        dirs = [str(paths.agent_dir), str(paths.verifier_dir), str(paths.artifacts_dir)]
        await self.ensure_dirs(list(dict.fromkeys(dirs + self._mount_targets(writable_only=True))))
        await self._upload_environment_dir_after_start()

    async def stop(self, delete: bool):
        if self._container_id is not None:
            await channel().request("detach")
            self._container_id = None
        if delete and not self._use_prebuilt:
            await self._docker(["image", "rm", self._image_name], check=False)

    def _require(self) -> str:
        if not self._container_id:
            raise RuntimeError("environment not started")
        return self._container_id

    async def exec(self, command: str, cwd: str | None = None, env: dict[str, str] | None = None,
                   timeout_sec: int | None = None, user: str | int | None = None) -> ExecResult:
        user = self._resolve_user(user)
        env = self._merge_env(env)
        args = ["exec"]
        effective_cwd = cwd or self.task_env_config.workdir
        if effective_cwd:
            args += ["-w", effective_cwd]
        for key, value in (env or {}).items():
            args += ["-e", f"{key}={value}"]
        if user is not None:
            args += ["-u", str(user)]
        # Same shell wrapper as Harbor's docker environment for Linux containers.
        args += [self._require(), "bash", "-c", command]
        return await self._docker(args, check=False, timeout_sec=timeout_sec)

    async def upload_file(self, source_path: Path | str, target_path: str):
        parent = str(Path(target_path).parent)
        await self.exec(f"mkdir -p {shlex.quote(parent)}", user="root")
        await self._docker(["cp", str(source_path), f"{self._require()}:{target_path}"])

    async def upload_dir(self, source_dir: Path | str, target_dir: str):
        await self.exec(f"mkdir -p {shlex.quote(target_dir)}", user="root")
        await self._docker(["cp", f"{Path(source_dir)}/.", f"{self._require()}:{target_dir}"])

    async def download_file(self, source_path: str, target_path: Path | str):
        Path(target_path).parent.mkdir(parents=True, exist_ok=True)
        await self._docker(["cp", f"{self._require()}:{source_path}", str(target_path)])

    async def download_dir(self, source_dir: str, target_dir: Path | str):
        Path(target_dir).mkdir(parents=True, exist_ok=True)
        await self._docker(["cp", f"{self._require()}:{source_dir.rstrip('/')}/.", str(target_dir)])


class CortexAgent(BaseAgent):
    """CortexCode, driven by the eval harness through the frame's chatflow.turn."""

    @staticmethod
    def name() -> str:
        return "cortexcode"

    def version(self) -> str | None:
        return os.environ.get("CCEVAL_CORTEX_VERSION") or "dev"

    async def setup(self, environment: BaseEnvironment) -> None:
        if not isinstance(environment, CortexContainerEnvironment):
            raise RuntimeError("CortexCode reaches the task container through its container plugin: "
                               "run it with --env cceval_tb:CortexContainerEnvironment")

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        ch = channel()
        try:
            reply = await ch.request("run", instruction=instruction)
        except asyncio.CancelledError:
            # Harbor's agent timeout: CortexCode must be stopped before the verifier runs.
            await asyncio.shield(ch.request("cancel"))
            raise
        usage = reply.get("usage") or {}
        context.n_input_tokens = usage.get("prompt_tokens")
        context.n_cache_tokens = usage.get("cached_tokens")
        context.n_output_tokens = usage.get("completion_tokens")
        context.metadata = {"session_id": reply.get("session_id"), "turn": reply.get("turn")}
