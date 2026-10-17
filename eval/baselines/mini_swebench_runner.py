"""Run mini-swe-agent's official SWE-bench flow on one instance against the local endpoint.

This is `mini-extra swebench` (minisweagent/run/benchmarks/swebench.py, process_instance) for one
instance: the shipped config/benchmarks/swebench.yaml (prompts, step_limit 250, 60 s per command,
docker environment with cwd /testbed and BASH_ENV), DefaultAgent, and the instance's SWE-bench image.
What differs, and why:
  - the model is the local OpenAI-compatible endpoint (litellm `openai/<model>` + api_base);
  - the instance row comes from the pinned dataset file, not from Hugging Face (same 500 rows);
  - the image is the one the harness grades this attempt in (--image; mini's own default is the
    published x86_64 image, which is also this harness's default), started with --platform so an
    x86_64 image runs under emulation on Apple Silicon without a platform warning;
  - the submission (mini's model_patch) is written to --patch-out; the eval harness grades it.

Control channel (fd 3 out, stdin in) as in mini_runner.py: one attempt (SWE-bench has no retries).
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

import yaml


def usage_totals(messages):
    prompt = completion = calls = 0
    for m in messages:
        resp = (m.get("extra") or {}).get("response")
        if isinstance(resp, dict) and isinstance(resp.get("usage"), dict):
            prompt += resp["usage"].get("prompt_tokens") or 0
            completion += resp["usage"].get("completion_tokens") or 0
            calls += 1
    return {"prompt_tokens": prompt, "completion_tokens": completion, "api_calls": calls}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--instance", required=True)
    ap.add_argument("--image", required=True)
    ap.add_argument("--platform", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--api-base", required=True)
    ap.add_argument("--trajectory", required=True)
    ap.add_argument("--patch-out", required=True)
    args = ap.parse_args()

    import minisweagent
    from minisweagent.agents.default import DefaultAgent
    from minisweagent.config import builtin_config_dir
    from minisweagent.models import get_model
    from minisweagent.run.benchmarks.swebench import get_sb_environment

    instance = json.loads(Path(args.instance).read_text())
    config = yaml.safe_load((builtin_config_dir / "benchmarks" / "swebench.yaml").read_text())
    model_cfg = dict(config.get("model", {}))
    model_kwargs = dict(model_cfg.get("model_kwargs", {}))
    model_kwargs.update(api_base=args.api_base, api_key="local", drop_params=True)
    model_cfg.update(model_name=args.model, model_kwargs=model_kwargs, cost_tracking="ignore_errors")
    env_cfg = dict(config.get("environment", {}))
    env_cfg["run_args"] = ["--rm", "--platform", args.platform]
    config["environment"] = env_cfg
    # get_swebench_docker_image_name uses the instance's image_name when present.
    instance = {**instance, "image_name": args.image}

    ctl = os.fdopen(3, "w", buffering=1)
    start = time.time()
    model = get_model(config=model_cfg)
    env = get_sb_environment(config, instance)
    agent_cfg = dict(config.get("agent", {}))
    agent_cfg["output_path"] = Path(args.trajectory)
    agent = DefaultAgent(model, env, **agent_cfg)
    exit_status, submission, error = None, "", None
    try:
        info = agent.run(instance["problem_statement"])
        exit_status = info.get("exit_status")
        submission = info.get("submission") or ""
    except Exception as e:  # recorded like process_instance does; the empty patch is graded as such
        exit_status, error = type(e).__name__, str(e)
    finally:
        agent.save(Path(args.trajectory), {"info": {"exit_status": exit_status, "submission": submission, "error": error},
                                           "instance_id": instance["instance_id"]})
        Path(args.patch_out).write_text(submission)
        env.cleanup()
    ctl.write(json.dumps({"event": "attempt_done", "attempt": 1, "duration": time.time() - start,
                          "exit_status": exit_status, **usage_totals(agent.messages)}) + "\n")
    json.loads(sys.stdin.readline())
    ctl.write(json.dumps({"event": "finished", "duration": time.time() - start, "exit_status": exit_status,
                          "mini_version": minisweagent.__version__, **usage_totals(agent.messages)}) + "\n")


if __name__ == "__main__":
    main()
