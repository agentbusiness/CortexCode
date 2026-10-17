"""Run mini-swe-agent (DefaultAgent + LocalEnvironment, the shipped `mini.yaml` prompts) on one task
against the local OpenAI-compatible endpoint.

Control channel (fd 3 out, stdin in) as in aider_runner.py. For the polyglot protocol the second
try continues the same conversation with the test output and aider's test_failures prompt, which
mirrors what aider's benchmark does for aider itself.
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

import yaml

TEST_FAILURES = """
####

See the testing errors above.
The tests are correct, don't try and change them.
Fix the code in {file_list} to resolve the errors.
"""


def run_until_exit(agent):
    """The loop body of DefaultAgent.run without its message reset (continues a conversation)."""
    from minisweagent.exceptions import FormatError, InterruptAgentFlow

    while True:
        try:
            agent.step()
            agent.n_consecutive_format_errors = 0
        except FormatError as e:
            agent.cost += e.messages[0].get("extra", {}).get("cost", 0.0)
            agent.n_consecutive_format_errors += 1
            if 0 < agent.config.max_consecutive_format_errors <= agent.n_consecutive_format_errors:
                agent.add_messages(*e.messages, {"role": "exit", "content": "RepeatedFormatError",
                                                 "extra": {"exit_status": "RepeatedFormatError", "submission": ""}})
            else:
                agent.add_messages(*e.messages)
        except InterruptAgentFlow as e:
            agent.add_messages(*e.messages)
        except Exception as e:
            agent.handle_uncaught_exception(e)
            break
        finally:
            agent.save(agent.config.output_path)
        if agent.messages[-1].get("role") == "exit":
            break
    return agent.messages[-1].get("extra", {})


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
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--api-base", required=True)
    ap.add_argument("--message-file", required=True)
    ap.add_argument("--trajectory", required=True)
    ap.add_argument("--file-list", default="")
    ap.add_argument("--tries", type=int, default=1)
    ap.add_argument("--command-timeout", type=int, default=0)
    args = ap.parse_args()

    import minisweagent
    from minisweagent.agents.default import DefaultAgent
    from minisweagent.environments.local import LocalEnvironment
    from minisweagent.models.litellm_model import LitellmModel

    cfg = yaml.safe_load((Path(minisweagent.__file__).parent / "config" / "mini.yaml").read_text())
    agent_cfg = {k: v for k, v in cfg["agent"].items() if k != "mode"}
    # No step or cost cap (cost is zero locally); the harness enforces the wall-clock budget.
    agent_cfg.update(step_limit=0, cost_limit=0.0, output_path=Path(args.trajectory))
    model_cfg = dict(cfg.get("model", {}))
    model_cfg.update(model_name=args.model, cost_tracking="ignore_errors",
                     model_kwargs={"api_base": args.api_base, "api_key": "local", "drop_params": True})
    env_cfg = dict(cfg.get("environment", {}))
    env_cfg["cwd"] = args.workspace
    if args.command_timeout:
        env_cfg["timeout"] = args.command_timeout
    model = LitellmModel(**model_cfg)
    env = LocalEnvironment(**env_cfg)
    agent = DefaultAgent(model, env, **agent_cfg)

    ctl = os.fdopen(3, "w", buffering=1)
    task = Path(args.message_file).read_text()
    start = time.time()
    outcomes = []
    for i in range(args.tries):
        if i == 0:
            try:
                info = agent.run(task)
            except Exception as e:
                info = {"exit_status": type(e).__name__, "error": str(e)}
        else:
            agent.add_messages(model.format_message(role="user", content=task))
            info = run_until_exit(agent)
        ctl.write(json.dumps({"event": "attempt_done", "attempt": i + 1, "duration": time.time() - start,
                              "exit_status": info.get("exit_status"), **usage_totals(agent.messages)}) + "\n")
        reply = json.loads(sys.stdin.readline())
        outcomes.append(bool(reply["passed"]))
        if "prompt" in reply:
            # Sequential suites (Web-Bench): the harness sends the next message verbatim, or null.
            if reply["prompt"] is None or i + 1 >= args.tries:
                break
            task = reply["prompt"]
            continue
        if reply["passed"] or i + 1 >= args.tries:
            break
        task = reply["feedback"] + TEST_FAILURES.format(file_list=args.file_list)
    agent.save(Path(args.trajectory))
    ctl.write(json.dumps({"event": "finished", "tests_outcomes": outcomes, "duration": time.time() - start,
                          "mini_version": minisweagent.__version__, **usage_totals(agent.messages)}) + "\n")


if __name__ == "__main__":
    main()
