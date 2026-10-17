"""Run one Terminal-Bench 2.0 trial with Harbor's reference agent (Terminus-2) on the local endpoint.

The trial is Harbor's own, end to end: `harbor trial start -p <task> --agent terminus-2` with Harbor's
docker environment (the task's prebuilt image, or environment/Dockerfile with --force-build), the
task's own agent and verifier timeouts, and Harbor's verifier. Terminus-2 calls the model through
LiteLLM on the host, so the local OpenAI-compatible endpoint is reachable as `openai/<model>` with
`api_base` (a Terminus-2 option) and `model_info` (context and output limits for LiteLLM).

Control channel (fd 3 out, stdin in) as in mini_runner.py: one attempt; the harness grades it by
reading the trial's result.json.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--harbor", required=True, help="harbor executable of the eval venv")
    ap.add_argument("--task-dir", required=True)
    ap.add_argument("--trials-dir", required=True)
    ap.add_argument("--trial-name", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--api-base", required=True)
    ap.add_argument("--model-info", required=True, help="JSON for Terminus-2's model_info")
    ap.add_argument("--force-build", action="store_true")
    ap.add_argument("--agent-timeout", type=float, default=None)
    ap.add_argument("--log", required=True)
    args = ap.parse_args()

    cmd = [args.harbor, "trial", "start", "-p", args.task_dir, "--agent", "terminus-2", "--model", args.model,
           "--agent-kwarg", f"api_base={args.api_base}", "--agent-kwarg", f"model_info={args.model_info}",
           "--trials-dir", args.trials_dir, "--trial-name", args.trial_name, "--no-delete"]
    if args.force_build:
        cmd.append("--force-build")
    if args.agent_timeout:
        cmd += ["--agent-timeout", str(args.agent_timeout)]
    ctl = os.fdopen(3, "w", buffering=1)
    start = time.time()
    with open(args.log, "a") as log:
        log.write("$ " + " ".join(cmd) + "\n")
        log.flush()
        code = subprocess.call(cmd, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
    result = {}
    try:
        result = json.loads((Path(args.trials_dir) / args.trial_name / "result.json").read_text())
    except (OSError, ValueError):
        pass
    agent = result.get("agent_result") or {}
    usage = {"prompt_tokens": agent.get("n_input_tokens"), "completion_tokens": agent.get("n_output_tokens"),
             "cached_tokens": agent.get("n_cache_tokens")}
    ctl.write(json.dumps({"event": "attempt_done", "attempt": 1, "duration": time.time() - start,
                          "harbor_exit": code, **usage}) + "\n")
    json.loads(sys.stdin.readline())
    ctl.write(json.dumps({"event": "finished", "duration": time.time() - start, "harbor_exit": code, **usage}) + "\n")


if __name__ == "__main__":
    main()
