"""Grade one LiveCodeBench solution with the official grader.

The grading code is LiveCodeBench's own (lcb_runner/evaluation/compute_code_generation_metrics.py
`check_correctness` -> testing_util.py `run_test`), copied byte-for-byte from the pinned commit into
the toolchain directory (eval/cache/toolchains/lcb/src). Official settings: timeout 6 s per test,
the global timeout check_correctness applies, all public + private tests in order, a problem passes
only if every test passes (graded_list all True, as in pass@1).

The tests file is read into memory and deleted from the scoring copy before any solution code runs,
so the solution cannot read the expected outputs from disk.

usage: python -I grade.py --solution solution.py --tests <tests.json> [--timeout 6]
Exit code 0 = all tests passed, 1 = failed, 2 = grader error. The last stdout line is a JSON verdict.
"""
import argparse
import json
import os
import sys

LCB_SRC = os.environ.get("CCEVAL_LCB_SRC")
if LCB_SRC:
    sys.path.insert(0, LCB_SRC)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--solution", required=True)
    ap.add_argument("--tests", required=True)
    ap.add_argument("--timeout", type=int, default=6)
    args = ap.parse_args()

    from lcb_runner.evaluation.compute_code_generation_metrics import check_correctness

    with open(args.tests, encoding="utf-8") as f:
        sample_obj = json.load(f)
    os.unlink(args.tests)
    try:
        with open(args.solution, encoding="utf-8") as f:
            code = f.read()
    except FileNotFoundError:
        print(json.dumps({"verdict": "fail", "reason": "solution file missing", "n_tests": len(sample_obj["inputs"])}))
        return 1
    sample = {"input_output": json.dumps(sample_obj)}
    results, metadata = check_correctness(sample, code, timeout=args.timeout, debug=False)
    fixed = []
    for e in results:
        if hasattr(e, "item"):
            e = e.item()
        fixed.append(bool(e) if isinstance(e, bool) or str(type(e)).endswith("bool_'>") else e)
    n = len(sample_obj["inputs"])
    passed = len(fixed) == n and all(x is True for x in fixed)
    verdict = {
        "verdict": "pass" if passed else "fail", "n_tests": n,
        "n_passed": sum(1 for x in fixed if x is True), "graded": fixed, "metadata": dict(metadata),
    }
    print(json.dumps(verdict, default=str))
    return 0 if passed else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except BaseException as e:  # grader failure, not a solution failure
        print(json.dumps({"verdict": "grader_error", "error": repr(e)}))
        sys.exit(2)
