Shell scripts here: get it right, run and check them, code that runs, common errors and fixes.

## 1. Get it right

1. **Pin down what the script must do.** Read the request, then what the project already says about it: the script, its callers (Makefile, CI), its tests and docs.
   - Note: arguments, input (stdin, files), exact output (stdout/stderr, format, final newline), exit status per case.
   - The request and the project decide, never a name alone. Keep names, options and the shebang's dialect.
   - "No result" in a script is a non-zero exit status with nothing on stdout (an error message goes to stderr), unless the request or the project names an output for it.
   - The request and the project disagree: say so and ask, instead of silently picking one.
   - Before coding, note the request's examples: they are the check, their values copied, never computed.
   - Unfamiliar tool, format or protocol: look it up (the research plugin, the man page, how others solved it):
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
2. **Write whole units, in slices.**
   - Simplest complete path first (one case, input to result). Run it.
   - Then each next behaviour, running after each.
   - A new script is written whole with `edit` `content`; an existing script is changed with anchored `edits` only, every change to it in one call.
   - Replace a function from `name() {` to its closing `}`. Never leave part of the old body, or a second definition (the last wins silently).
3. **Run it after every change** on a sample input; compare output and exit status:

```bash
bash -n x.sh && printf 'a b\nc\n' | bash x.sh arg1; echo "exit=$?"
```

4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests if any (bats, a test script, `make test`); fix the first failure, run again.
5. **Before you finish**, check each requirement again, error messages and exit codes included.

## 2. Run, test, check

```bash
bash -n x.sh                    # syntax only; exit 2 on a syntax error
shellcheck -x x.sh              # lint, following sourced files; exit 1 = findings
shellcheck -s sh x.sh           # a #!/bin/sh script: SC3xxx = not POSIX
bash x.sh args; echo "exit=$?"  # its set -e, cd and exit stay inside the script
```

- `run` is a persistent bash with empty stdin.
- Never type `set -e`, `set -u` or `exit` in `run` itself: they stay on, or end the shell.
- A prompting script gets EOF: pipe answers (`printf 'y\n' | bash x.sh`) or use `terminal`.
- ShellCheck findings (`SC2086`) come after each edit and from `read diagnostics://x.sh`. Fix those in your lines, parse errors (SC1xxx) first. Obey `.shellcheckrc`.
- No shellcheck: `bash -n`, then run it.
- This Mac's `/bin/bash` is 3.2: no `declare -A`, `mapfile`, `${s,,}`, `${a[-1]}`. Under `set -u` an empty `"${a[@]}"` is unbound.
- Linux-only scripts (CI, containers) may use bash 4+; say they cannot run here.
- `./x.sh` needs `chmod +x`; `sh x.sh` runs it as POSIX sh.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: `tests/test_<name>.sh` that runs the script with inputs and compares its output, run with `bash tests/test_<name>.sh`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `printf '0 %.0s' $(seq 18)`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `[ "$got" = "$want" ] || echo "got: $got"`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/`; git ignores `tmp/`.

## 3. Code that runs

```bash
#!/usr/bin/env bash
set -euo pipefail
```

`#!/bin/sh`: `set -eu` only (dash rejects `pipefail`), no `[[`, no arrays, `=` not `==`.

Quote every expansion:

```bash
cp $src $dst                    # bad: splits on spaces, expands globs
cp -- "$src" "$dst"             # good
cmd $*                          # bad: re-splits the arguments
cmd "$@"                        # good
```

`[` is a command: quote its operands. `[[` (bash) does not split; `==` matches patterns; `=~` takes an unquoted regex:

```bash
[ $x = yes ]                    # bad: fails when x is empty or has spaces
[ "$x" = yes ]                  # good; bash: [[ $x == yes* ]]
```

Files and lines without splitting:

```bash
for f in $(ls *.txt)            # bad: breaks on spaces
for f in ./*.txt; do [ -e "$f" ] || continue; ...; done
while IFS= read -r line || [ -n "$line" ]; do ...; done < file   # keeps spaces, backslashes, last line
```

Failures must not pass silently:

```bash
cd "$dir"; rm -rf ./*           # bad: a failed cd deletes the wrong directory
cd "$dir" || exit 1             # good
rm -rf "${dir:?}/"*             # an empty $dir would mean /*
local out=$(cmd)                # bad: local's status 0 hides cmd's failure
local out; out=$(cmd)           # good
curl -s "$url" | jq .name       # bad without pipefail: exit 0 when curl fails
grep -c pat f || true           # grep exits 1 on no match: allow it only where that is fine
```

- A pipe into `while` runs a subshell; its variables are lost. Use `done < <(cmd)`.
- `((i++))` returns 1 when `i` was 0, which stops `set -e` in bash 4+ (CI). Use `i=$((i + 1))`.

**BSD (this Mac) vs GNU (Linux):**

| Task | macOS | Linux | Both |
|---|---|---|---|
| in-place edit | `sed -i '' 's/a/b/' f` | `sed -i 's/a/b/' f` | `sed -i.bak 's/a/b/' f && rm f.bak` |
| date | `date -j -f %Y-%m-%d 2024-01-01 +%s`, `date -v-1d +%F` | `date -d 2024-01-01 +%s` | `python3 -c` with datetime |
| script's directory | `readlink -f` only on recent macOS | `readlink -f "$0"` | `cd "$(dirname "$0")" && pwd -P` |
| time limit | no `timeout` | `timeout 60 cmd` | `run`'s `timeout_ms` |

## 4. Errors

| Error | Fix |
|---|---|
| `syntax error: unexpected end of file` | unclosed `if`, `for`, `case`, `{`, `$(` or quote; `shellcheck x.sh` points at the opening token |
| ``syntax error near unexpected token `fi'`` | missing `;` before `then`/`do`, or an empty block (put `:` in it) |
| `$'\r': command not found`, `bad interpreter` | CRLF endings: `perl -pi -e 's/\r$//' x.sh` |
| `command not found` (127) | not installed or a typo: `command -v x` |
| `VAR: unbound variable` | `set -u`: `"${1:-}"`, `"${VAR:-default}"`, `"${VAR:?VAR is required}"` |
| `[: too many arguments`, `unary operator expected` | unquoted variable inside `[ ]`: quote it |
| `integer expression expected` | not a number, often a trailing `\r`: `printf '%q\n' "$v"` shows it |
| `bad substitution`, `declare: -A: invalid option` | bash 4 feature in bash 3.2, or bash syntax run by `sh` |
| `sed: 1: "f": invalid command code f`, `date: illegal option -- d` | GNU flags on macOS: the table above |
| exit 141 | SIGPIPE from `cmd \| head` under pipefail: add `\|\| true` |
| stops with no message | `set -e` hit a failing command: `bash -x`, read the last `+` line |

ShellCheck codes:
- SC1009, SC1072, SC1073: parse error at that token. SC1017: CRLF.
- SC2086, SC2046: quote it. SC2048, SC2068: `"$@"`.
- SC2012, SC2045: a glob, not `ls`.
- SC2030, SC2031: variable lost in a subshell.
- SC2115: `"${dir:?}"`. SC2155: split `local` and assignment.
- SC2162: `read -r`. SC2164: `cd dir || exit 1`.
- SC1090, SC1091: `# shellcheck source=lib.sh` above the `source`.
- SC3010, SC3014, SC3030, SC3040, SC3054: bash feature in `sh`: POSIX form or a bash shebang.
