Python here: get it right, run and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the task statement and the code (signatures, stubs, tests).
   - Per function: what it takes, returns and raises.
   - The statement decides, never the name. "whether" returns a bool. "find" may return `None`. "compute X" returns X, not whether X exists.
   - When there may be no answer (nothing found, no valid result) and the statement does not name a value for that case, return `None`: it is Python's "no result". Not `False`, `-1` or `[]`, and not a value remembered from elsewhere.
   - Keep existing names and signatures.
2. **Before coding, turn the statement into cases** for the check script (section 2).
   - Each sentence that states a behaviour, value, error or limit is one case. A shown requirement list is that list.
   - A sentence with no case is a requirement you will miss.
   - Unsure of a domain, format or protocol? With the research plugin, learn its rules and copy them into the cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that states them.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the check script.
   - Add the next behaviour the statement names. Run again after every change.
   - New file, or a stub you have not changed yet: send it whole once with `edit` `content`. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a function: replace its whole `def` block, `def` line to last line. Never leave part of the old body.
4. **Run the project's tests** (section 2). Fix the first failure's cause, run again. Change only the code that failure runs through: a function whose cases pass stays as it is unless the task asks to change it.
5. **Before you finish**, check each requirement against the code, including the named errors (type and message).

A check disagrees with the statement: the statement wins. Fix the code, name the wrong check.

## 2. Run, test, check

`python` and `pip` are the session's venv. Call tools as `python -m <tool>`.

```bash
python file.py                                   # run a script
python -m py_compile mod.py                      # syntax check, no output when fine
python -m pytest -q -x                           # all tests, stop at the first failure
python -m pytest tests/test_mod.py -q            # one file
python -m pytest "tests/test_mod.py::test_x" -q  # one test
python -m unittest -q tests.test_mod             # a project without pytest
```

- pytest missing: `python -m pip install pytest` (offline: `uv pip install --offline pytest`), or use `python -m unittest`.
- Never install into the system Python, never `sudo`, never create another venv.
- Project's own package not found: `python -m pip install -e .` (offline: `uv pip install --offline -e .`), or prefix the command with `PYTHONPATH=src` for a `src/` layout.
- CI files (`.github/workflows/*.yml`, `Makefile`) show the project's exact test command.

**Checking your work.** One file, `check.py`, beside the code. Edit it; never re-send it as a heredoc.
- Build inputs in code, never by hand.
- Copy expected values from the statement, never work them out yourself.
- Print one line per case, no asserts: one run shows every case.

```python
from mymodule import solve

cases = [                      # (input built in code, result copied from the statement)
    ([3, 1, 2], [1, 2, 3]),
    (list(range(5)) * 2, list(range(5)) * 2),
]
for given, want in cases:
    try:
        got = solve(given)
    except Exception as error:
        got = f"{type(error).__name__}: {error}"
    print("ok " if got == want else "BAD", given, "->", got, "want", want)
```

Run `python check.py`. Fix the first `BAD`, run again.
- After any fix, run every case again, passing ones too. One fix can break another case.
- A case fails: check the case before the code. Re-derive it from the statement. A case that breaks a stated rule or holds a value you computed is bad: fix it first.
- Keep the file while the work can change. Delete it when the user is done.

## 3. Code that runs

Raise the exact type and message the statement names:

```python
raise ValueError("width must be positive")   # good
raise Exception("bad input")                 # bad: a test expecting ValueError fails
```

Return values, never print them:

```python
def total(items): print(sum(items))     # bad: returns None
def total(items): return sum(items)     # good
```

Methods that mutate return `None`:

```python
xs = xs.sort()          # bad: xs is now None
xs.sort()               # good, or xs = sorted(xs)
```

Mutable default arguments are shared between calls:

```python
def add(x, seen=[]): ...                   # bad
def add(x, seen=None):
    seen = [] if seen is None else seen    # good
```

- `/` is float division; `//` for integers. Compare floats with `math.isclose`.
- Do not change a list while iterating it; build a new one.
- An iterator is used up after one pass; make a list to reuse it.
- Do not name variables `list`, `dict`, `id`, `input`, `type`.
- Recursion fails near depth 1000. Use a loop or an explicit stack.
- Indent with 4 spaces. After re-indenting a block, read the lines back.

## 4. Errors

| Error | Usual cause and fix |
|---|---|
| `IndentationError`, `SyntaxError` | a broken edit: read the lines around it and fix them |
| `NameError: name 'x' is not defined` | missing import, typo, or wrong scope |
| `ModuleNotFoundError` | missing dependency (section 2) or project package not importable |
| `ImportError: cannot import name` | the name is not there; check spelling and circular imports |
| `AttributeError: 'NoneType' object has no attribute` | a function returned `None` (mutating method, missing `return`) |
| `TypeError: f() missing 1 required positional argument` | call does not match the signature; read the `def` |
| `TypeError: unsupported operand type(s)` | mixed types (`str` + `int`); convert explicitly |
| `KeyError`, `IndexError` | key or index not there; check the data first |
| `RecursionError` | no base case reached, or input too deep; use a loop |
| `AssertionError` in pytest | read the `E` lines: both compared values |
