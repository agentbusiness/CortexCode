A command may hang, run long or flood output, or a server or watcher must run in the background.

Programs that wait for keystrokes (REPL, debugger, prompt, editor): `skill://skills-terminal/interactive-terminal`.

## Where commands run

- Every `run` starts in the repository root. Use paths relative to it.
- `cd` and `export` carry over to later calls: a `cd` changes every later relative path.
- One command elsewhere: `run`'s `cwd`, or `(cd sub && cmd)`. Unsure: `pwd`.
- After a timeout, cancel or exit, the shell restarts in the saved directory with exported variables. Plain variables, functions and `set` options are lost.
- Never type `set -e`, `set -u` or `exit` directly. Use a script, or `( set -euo pipefail; ... )`.
- Stdin is empty: a question gets EOF or waits until the timeout. Answer up front: `-y`, `git --no-pager log -20`, `git commit -m "..."`, `GIT_EDITOR=true git rebase --continue`, `cmd < input.txt`.
- In a containerized project, `run` executes inside the container. Same rules.

## Before running

1. Know the kind: one-shot (build, test, script), long job, or server/watcher that never exits. For project commands, read the Makefile target or package script first.
2. A one-shot that might hang (network, lock, deadlock): `timeout_ms` (e.g. `120000`), not the 10-minute default. This Mac has no `timeout` command.
3. Server, watcher, or job over ~10 minutes: `background: true` (below).
   - Never a server in a plain `run`: it blocks until the timeout.
   - Never `cmd &` or `nohup`: output leaks into later results, nothing wakes you, the next timeout kills it.

After a timeout, find the cause (last output, a prompt, a lock, a missing service) before rerunning.

## Output

- The result ends with `[exit N, Ts]`.
- Long output: head and tail, plus `artifact://<id>` saved whole. Errors are usually in the tail. Other parts: `read artifact://<id>:200-260`.
- When the exit status matters, never pipe into `head`, `tail` or `grep` (`cmd | tail` reports tail's status). Filter a saved log:

```bash
make test > /tmp/test.log 2>&1; echo "exit=$?"; grep -n -m20 -E 'error|FAIL' /tmp/test.log
```

## Background jobs and servers

`run` with `background: true` returns a job id at once. You are woken when the job exits. `read artifact://<id>` (from the start message) shows its output so far.

- Ready line: `wake: {"pattern": "Listening on|ready"}`.
- Long builds: `wake: {"silence_s": 300}` wakes you when output stops.
- Or a bounded poll that prints what it found:

```bash
for i in $(seq 30); do curl -fsS -o /dev/null 127.0.0.1:8000/ && echo up && break; sleep 1; done
```

- Not up: read the job's output before changing anything.
- `Address already in use`: `lsof -nP -iTCP:8000 -sTCP:LISTEN`; stop the holder or use another port.
- `cx job status <id>`, `cx job list`, `cx job kill <id>`.
- Stop every job you started before you finish, unless the task wants it running.
- Reached from a container or another machine: bind `0.0.0.0`, not `127.0.0.1`.

## Never wait silently

- No bare `sleep 60`. No long foreground command with hidden output.
- Say in `intent` what you wait for. Wait with a wake rule or a bounded, printing poll.
- While a background job runs, do the next useful step (read code, write the test); do not poll it.

## Exit codes

| Exit | Meaning |
|---|---|
| 1, 2 | failure, usage error (grep: 1 = no match, 2 = error) |
| 126 | not executable: `chmod +x`, or a CRLF shebang |
| 127 | command not found: not installed, not on PATH, or a typo (`command -v x`) |
| 130, 143 | interrupted, terminated |
| 137 | killed: out of memory, or `kill -9` |
| 141 | SIGPIPE: the reader (`head`) stopped early; usually harmless |
