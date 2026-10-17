"""Grade one RustEvo² task with the benchmark's own evaluation procedure.

From RustEvo Evaluate/eval_models_rq1.py at the pinned commit (copied verbatim below, comments
translated): `create_test_file` joins the solution and the task's test program into one crate root;
`run_rust_test` compiles it with `rustup run <to_version> rustc --test` (30 s timeout) and runs the
test binary (30 s timeout). Here the toolchain's rustc is invoked directly from the eval-owned
RUSTUP_HOME (that is exactly what the rustup proxy executes). `check_function_signature` and
`check_api_usage` are RustEvo's name checks that its RQ1 script applies before running the tests;
their outcome is recorded as `official_prechecks`, but the verdict (exit code) is test execution only.

The hidden test program is read into memory and deleted from the scoring copy before compiling.

usage: python -I grade.py --solution src/lib.rs --task <task-test.json> --rustc <path to rustc>
Exit code 0 = compiled and all tests passed, 1 = failed, 2 = grader error. Last stdout line: JSON verdict.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from typing import Tuple


# ---- verbatim from RustEvo Evaluate/eval_models_rq1.py ----
def check_function_signature(code: str, signature: str) -> bool:
    """Check if the generated code contains a function matching the required signature."""
    import re

    # If the signature or the code is empty, return False
    if not signature or not code:
        return False

    # Clean the signature: remove attributes, doc comments and visibility modifiers
    clean_signature = re.sub(r'#\[.*?\]', '', signature)
    clean_signature = re.sub(r'///.*?\n', '', clean_signature)
    clean_signature = re.sub(r'//.*?\n', '', clean_signature)
    clean_signature = re.sub(r'pub\s+', '', clean_signature).strip()

    # Extract the function name
    fn_match = re.search(r'fn\s+(\w+)', clean_signature)
    if not fn_match:
        return False

    fn_name = fn_match.group(1)

    # Basic check: the code must contain this function name
    if not re.search(r'fn\s+' + re.escape(fn_name) + r'\s*[(<]', code):
        return False

    # Looser check: a matching function name is considered a correct signature
    return True


def check_api_usage(code: str, api_name: str) -> bool:
    """Check if the generated code uses the specified API."""
    import re

    # If the API name or the code is empty, return False
    if not api_name or not code:
        return False

    # Last part of the API name (function/method name)
    base_api_name = api_name.split('::')[-1] if '::' in api_name else api_name

    full_match = re.search(r'\b' + re.escape(api_name) + r'\b', code)
    base_match = re.search(r'\b' + re.escape(base_api_name) + r'\b', code)

    if full_match or base_match:
        return True
    else:
        return False


def create_test_file(code: str, test_program: str) -> str:
    """Combine code solution and test program into a complete test file."""
    # Wrap test program in appropriate module if not already done
    if "#[cfg(test)]" not in test_program:
        test_program = f"""
#[cfg(test)]
mod tests {{
    use super::*;

    {test_program}
}}
"""

    return f"{code}\n\n{test_program}"
# ---- end verbatim ----


def run_rust_test(test_file_content: str, rustc: str, workdir: str) -> Tuple[bool, str, dict]:
    """run_rust_test from eval_models_rq1.py, with the toolchain's rustc and no shell."""
    src = os.path.join(workdir, "main.rs")
    out = os.path.join(workdir, "main")
    with open(src, "w", encoding="utf-8") as f:
        f.write(test_file_content)
    info = {}
    try:
        c = subprocess.run([rustc, "--test", src, "-o", out], capture_output=True, text=True, timeout=30)
        info["compile_exit"] = c.returncode
        info["compile_stderr"] = c.stderr[-6000:]
        if c.returncode != 0:
            return False, "Compilation error: " + c.stderr, info
        t = subprocess.run([out], capture_output=True, text=True, timeout=30, cwd=workdir)
        info["test_exit"] = t.returncode
        info["test_stdout"] = t.stdout[-6000:]
        info["test_stderr"] = t.stderr[-3000:]
        if t.returncode != 0:
            return False, "Test execution error: " + t.stderr, info
        return True, "", info
    except subprocess.TimeoutExpired as e:
        info["timeout"] = str(e.cmd[0] if isinstance(e.cmd, list) else e.cmd)
        return False, "Timeout: Test took too long to execute", info


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--solution", required=True)
    ap.add_argument("--task", required=True)
    ap.add_argument("--rustc", required=True)
    args = ap.parse_args()
    with open(args.task, encoding="utf-8") as f:
        task = json.load(f)
    os.unlink(args.task)
    try:
        with open(args.solution, encoding="utf-8") as f:
            code = f.read()
    except FileNotFoundError:
        print(json.dumps({"verdict": "fail", "reason": "solution file missing"}))
        return 1
    prechecks = {
        "signature": check_function_signature(code, task["function_signature"]),
        "api_usage": check_api_usage(code, task["api_name"]),
    }
    with tempfile.TemporaryDirectory(prefix="rustevo-") as d:
        ok, err, info = run_rust_test(create_test_file(code, task["test_program"]), args.rustc, d)
    verdict = {
        "verdict": "pass" if ok else "fail", "error": err[:3000], "official_prechecks": prechecks,
        "official_pass": ok and prechecks["signature"] and prechecks["api_usage"], **info,
    }
    print(json.dumps(verdict))
    return 0 if ok else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except BaseException as e:
        print(json.dumps({"verdict": "grader_error", "error": repr(e)}))
        sys.exit(2)
