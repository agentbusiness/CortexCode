"""Run aider on one task, following aider's own benchmark protocol (benchmark/benchmark.py,
run_test_real at the pinned aider commit) with the harness doing the scoring between tries.

Control channel: JSON lines written to fd 3; harness replies on stdin.
  -> {"event": "attempt_done", "attempt": i, ...stats}
  <- {"passed": bool, "feedback": "<cleaned test output>"}
  -> {"event": "finished", ...stats}

Modes:
  polyglot: the supplied solution files are added to the chat, other file mentions are ignored,
            up to `tries` attempts; after a failed try the test output plus aider's test_failures
            prompt is sent (exactly as benchmark.py does).
  message:  one message in a git repository (realistic suite); aider's repo map finds files and
            `yes=True` lets it add the files it asks for.

Sequential suites (--sequential, Web-Bench): the harness reply also carries "prompt", the next
message verbatim (the next step's task, or a retry with the test errors), or null to stop; the
same conversation continues across steps, up to `tries` messages.
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

TEST_FAILURES = """
####

See the testing errors above.
The tests are correct, don't try and change them.
Fix the code in {file_list} to resolve the errors.
"""


def control():
    return os.fdopen(3, "w", buffering=1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--edit-format", default="diff")
    ap.add_argument("--mode", choices=["polyglot", "message"], required=True)
    ap.add_argument("--message-file", required=True)
    ap.add_argument("--fnames", nargs="*", default=[])
    ap.add_argument("--ignore", nargs="*", default=[])
    ap.add_argument("--tries", type=int, default=2)
    ap.add_argument("--history", required=True)
    ap.add_argument("--model-metadata", default=None)
    ap.add_argument("--sequential", action="store_true")
    args = ap.parse_args()

    from aider import models
    from aider.coders import Coder
    from aider.io import InputOutput

    if args.model_metadata:
        models.register_litellm_models([args.model_metadata])

    ctl = control()
    os.chdir(args.workspace)
    io = InputOutput(pretty=False, yes=True, chat_history_file=args.history)
    main_model = models.Model(args.model, weak_model=None, editor_model=None, editor_edit_format=None)
    fnames = [str(Path(args.workspace) / f) for f in args.fnames]
    file_list = " ".join(Path(f).name for f in args.fnames)
    if args.mode == "polyglot":
        coder = Coder.create(
            main_model, args.edit_format, io, fnames=fnames, use_git=False, stream=False,
            cache_prompts=True, suggest_shell_commands=False, ignore_mentions=set(args.ignore),
        )
        coder.get_file_mentions = lambda x: set()  # No loading of any other files (benchmark.py)
    else:
        coder = Coder.create(
            main_model, args.edit_format, io, fnames=[], use_git=True, auto_commits=False,
            stream=False, cache_prompts=True, suggest_shell_commands=False,
        )
    instructions = Path(args.message_file).read_text()
    tries = args.tries if (args.mode == "polyglot" or args.sequential) else 1
    outcomes = []
    dur = 0.0
    for i in range(tries):
        start = time.time()
        error = None
        try:
            coder.run(with_message=instructions, preproc=False)
        except Exception as e:  # recorded; the attempt is still scored as the workspace stands
            error = f"{type(e).__name__}: {e}"
        dur += time.time() - start
        stats = {
            "attempt": i + 1, "duration": dur, "error": error,
            "prompt_tokens": coder.total_tokens_sent, "completion_tokens": coder.total_tokens_received,
            "num_malformed_responses": coder.num_malformed_responses,
            "num_exhausted_context_windows": coder.num_exhausted_context_windows,
        }
        ctl.write(json.dumps({"event": "attempt_done", **stats}) + "\n")
        reply = json.loads(sys.stdin.readline())
        outcomes.append(bool(reply["passed"]))
        if "prompt" in reply:
            if reply["prompt"] is None or i + 1 >= tries:
                break
            instructions = reply["prompt"]
            continue
        if reply["passed"] or i + 1 >= tries:
            break
        instructions = reply["feedback"] + TEST_FAILURES.format(file_list=file_list)
    ctl.write(json.dumps({
        "event": "finished", "tests_outcomes": outcomes, "duration": dur,
        "prompt_tokens": coder.total_tokens_sent, "completion_tokens": coder.total_tokens_received,
        "edit_format": coder.edit_format, "model": main_model.name,
    }) + "\n")


if __name__ == "__main__":
    main()
