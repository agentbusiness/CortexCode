A bug to find: wrong result without an error, flaky or hanging test, regression; tests to pin it.

Do the steps in order.

## 1. Reproduce with the smallest run

One test, or a snippet with the smallest failing input:

```bash
timeout 120 <one-test-command> > "${TMPDIR:-/tmp}/repro.log" 2>&1; echo "exit=$?"
```

- Long output comes back whole as `artifact://<id>`.
- Exit 124 = it hung: see Hangs.
- Cannot reproduce: change one difference from the report at a time (input, env vars, cwd, data files, time zone, test order).

## 2. Read the error

- Read the FIRST error and its innermost project stack frame.
- Find where a message is raised: `git grep -n "<fixed part of the message>"`.
- Frame in a dependency: `read definition://<path:line:col>` on the call.
- Wrong result: write expected and actual for one input. Expected comes from the task, a test or a hand calculation, never from running the code.
- Wrong for some inputs only: compare a failing and a passing input field by field.

## 3. Prove this code is the code that runs

Add a marker at the function's top, rerun: `print("DBG0", file=sys.stderr)`, `eprintln!("DBG0")`, `console.error("DBG0")`.

No marker printed:
- another copy is imported: `python -c "import pkg; print(pkg.__file__)"`, `node -p "require.resolve('pkg')"`;
- stale build or server: rebuild, restart;
- or the path is never reached.

## 4. Form one hypothesis

Trace the wrong value back: `read graph://used-by/<name>`, `references://<path:line:col>` (every write), `graph://path/<name>/<name>`.

Name a line and a value: "`end` is exclusive at dates.py:42, so the last day is dropped".

## 5. Test it

- Few values: numbered stderr prints (`DBG1 end=...`). Check midway: correct means the bug is later.
- Many values or loop state: a debugger in the `terminal` tool, never in `run` (its prompt hangs there). Python:
  1. Put `breakpoint()` on the line. Venv path: `run` `echo $VIRTUAL_ENV`.
  2. `terminal` `open` program `<venv>/bin/python -m pytest -s -x "tests/test_mod.py::test_x"`, `until` `\(Pdb\) \x{258C}`. Or no edit: `--pdb` stops at the failing assertion, `--trace` at the test's start.
  3. `send` a command plus `<Enter>`: `p <expr>`, `pp locals()`, `n` (next), `s` (step in), `r` (return), `c` (continue), `w` (stack), `u`/`d` (frame up/down). End: `q`, `close`.

  Other languages: the language guide.

Confirmed: step 6. Not: revert, note what it ruled out, next hypothesis. Change one thing at a time.

## 6. Fix the cause

- Fix the line that makes the bad value, not the symptom (no null check, `try/except` or special case).
- Same pattern elsewhere: `git grep -n "<pattern>"`.
- Never weaken an assertion, tolerance, snapshot or expected output, unless the task asks.

## 7. Add or keep a test

- Keep the repro as a test beside the code's tests (`read graph://impact/<name>`). Reuse their fixtures; go through the public interface.
- It fails before the fix for the reported reason and passes after.
- Run the module's whole suite.

## 8. Clean up

```bash
git grep -n -e DBG -e 'breakpoint()' -e 'debugger;'   # must print nothing
git diff --stat                                         # only the fix and its test
```

**Three fixes failed: stop patching, reduce to the core, then research.**
1. Verify your assumptions: the code that runs, its input, its config, the expected value.
2. Reduce to the core: the smallest failing input, one sentence "happens X, should Y".
3. Research plugin available: `read research://search/<sentence>?sources=github_issues,web&limit=5`, then `read research://fetch/<url>` for two sources.
4. Return with what you learned.

## Rules you cannot enumerate: a differential check

For rules, not examples (a rule system, a parser, a state machine): hand-computed expectations go wrong.

1. Write a second, plain implementation of the rules: slow, direct, no cleverness.
2. Generate inputs in code that obey the rules.
3. Compare both on thousands of inputs; print the first mismatches:

```
for each generated input:
    if reference(input) != solution(input): print(input, reference(...), solution(...))
```

- Mismatch: suspect the generator or reference first, the solution second.
- Zero mismatches is strong evidence. Keep the script while the work can still change; delete it when the user is done.

## Regression: it used to work

Bisect in a throwaway worktree; the check script lives outside the repo:

```bash
cat > "${TMPDIR:-/tmp}/bisect.sh" <<'EOF'
#!/bin/sh
<build-command> || exit 125      # 125 = cannot test this commit: skip it
<one-test-command>               # exit 0 = good, 1-124 = bad
EOF
chmod +x "${TMPDIR:-/tmp}/bisect.sh"
git worktree add --detach "${TMPDIR:-/tmp}/bisect" HEAD && cd "${TMPDIR:-/tmp}/bisect"
git bisect start <bad-sha> <good-sha>
git bisect run "${TMPDIR:-/tmp}/bisect.sh"
git bisect log | tail -3; git bisect reset
cd - && git worktree remove --force "${TMPDIR:-/tmp}/bisect"
```

Read the first bad commit: `git show --stat <sha>`, `git show <sha> -- <file>`.

## Hangs

Make the process show where it waits:

```bash
timeout -s ABRT 60 python -X faulthandler -m pytest -x "tests/test_mod.py::test_x"   # Python: dumps every thread's stack
go test -timeout 60s -run '^TestX$' ./pkg                                            # Go: dumps goroutines on expiry
jstack <pid>                                                                          # Java
lldb -p <pid> --batch -o 'thread backtrace all'                                       # native code on macOS
ps -eo pid,stat,etime,args | grep -i '[n]ame'                                         # R = spinning, S = waiting
```

Usual causes: reading stdin, an unreleased lock, an unresolved `await`, a network call without timeout.

## Flaky tests

Measure alone, then with its whole file:

```bash
for i in $(seq 30); do <one-test-command> > /dev/null 2>&1 || echo "fail $i"; done
```

- Fails only in the suite: an earlier test leaks state (env vars, globals, patches, files). Run that pair alone to confirm.
- Fails alone too: timing, unseeded randomness, the clock (try another `TZ=`), unordered collections.
- Fix the cause (await the event, reset state, inject clock or seed), never a retry or longer sleep.
- Done at 30 of 30, alone and in the suite.
