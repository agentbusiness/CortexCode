"""Grade one BigCodeBench solution with the official checker.

The checker is BigCodeBench's own `bigcodebench.eval.untrusted_check` (copied byte-for-byte from the
pinned commit into eval/cache/toolchains/bcb/src), called exactly as `bigcodebench.evaluate` does
with its defaults: calibrated solution (code_prompt + "\\n    pass\\n" + solution), min_time_limit =
1 s, gt_time_limit = 20 s (the official fallback; the effective timeout is TIMEOUT_LIMIT 240 s + 1
either way). One deviation: the memory rlimits are disabled (see the call below; macOS kernel). The task's
unittest TestCases run in a child process; the task passes only with status "pass".

The hidden task file (test code, entry point, code prompt) is read into memory and deleted from the
scoring copy before the solution runs.

usage: python -I grade.py --solution solution.py --task <task-test.json>
Exit code 0 = pass, 1 = fail/timeout, 2 = grader error. The last stdout line is a JSON verdict.
"""
import argparse
import json
import os
import sys

BCB_SRC = os.environ.get("CCEVAL_BCB_SRC")
if BCB_SRC:
    sys.path.insert(0, BCB_SRC)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--solution", required=True)
    ap.add_argument("--task", required=True)
    ap.add_argument("--no-calibrate", action="store_true")
    args = ap.parse_args()

    from bigcodebench.eval import untrusted_check

    with open(args.task, encoding="utf-8") as f:
        task = json.load(f)
    os.unlink(args.task)
    try:
        with open(args.solution, encoding="utf-8") as f:
            solution = f.read()
    except FileNotFoundError:
        print(json.dumps({"verdict": "fail", "status": "fail", "details": {"ALL": "solution file missing"}}))
        return 1
    if not args.no_calibrate:
        # bigcodebench/evaluate.py: `if calibrated: solution = code_prompt + "\n    pass\n" + solution`
        solution = task["code_prompt"] + "\n    pass\n" + solution
    # evaluate() defaults are max_as_limit = max_data_limit = 30*1024 MB, max_stack_limit = 10 MB. The
    # macOS kernel rejects RLIMIT_AS / RLIMIT_DATA below a process's existing virtual size (every
    # Python process on Apple silicon already reserves far more than 30 GB), so reliability_guard's
    # setrlimit raises and the child dies before running any test. BigCodeBench's own switch for
    # this is a zero limit (`if max_as_limit and max_data_limit and max_stack_limit:` skips them);
    # the stack limit is already skipped on Darwin upstream. Physical memory (48 GB) is the bound.
    stat, details = untrusted_check(
        solution, task["test"], task["entry_point"],
        0, 0, 10,  # max_as_limit, max_data_limit (disabled, see above), max_stack_limit
        1,  # min_time_limit (evaluate() default)
        20,  # gt_time_limit fallback (effective timeout is TIMEOUT_LIMIT 240 s + 1 either way)
    )
    details = {k: (v if len(str(v)) < 4000 else str(v)[:4000] + "...[truncated]") for k, v in dict(details).items()}
    print(json.dumps({"verdict": "pass" if stat == "pass" else "fail", "status": stat, "details": details}, default=str))
    return 0 if stat == "pass" else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except BaseException as e:
        print(json.dumps({"verdict": "grader_error", "error": repr(e)}))
        sys.exit(2)
