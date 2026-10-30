Python here: get it right, run and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request and the code it changes (signatures, docstrings) and its tests. Callers only when a signature changes; then write.
   - Per function: what it takes, returns and raises.
   - The request and the project decide, never a name alone. "whether" returns a bool. "find" may return `None`. "compute X" returns X, not whether X exists.
   - When there may be no answer and nothing names a value for that case, do what the project already does (`None`, an exception, an empty result); in new code `None` is Python's "no result", not `False` or `-1`.
   - Keep existing names and signatures.
   - The request and the project disagree: say so and ask, instead of silently picking one.
2. **Before coding, note the request's examples: they are the check, their values copied, never computed.**
   - Each behaviour, value, error or limit the request names is one case. A shown requirement list is that list.
   - Unsure of a domain, format or protocol? Look it up: the research plugin, the docs, how others solved it:
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the tests.
   - Add the next behaviour. Run again after every change.
   - A new file is written whole with `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a function: replace it from its `def` line to its last line. Never leave part of the old body.
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests (section 2); fix the first failure's cause, run again.
5. **Before you finish**, check each requirement against the code, including the errors the request or the project names (type and message).

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- Returning a stored list, dict or object hands out the object itself: the caller can change it. Return a copy (`list(x)`, `dict(x)`, `copy.copy(x)`) when it must not.
- A fixed record: `@dataclass` (`frozen=True` when it must not change). A state with several values: `enum.Enum`, not flags that can conflict.
- Shared between threads: a `threading.Lock` held around every change, or a `queue.Queue` between them.
- Containers: `dict` for lookup, `list` for order, `collections.deque` for both ends, `heapq` for smallest first, `set` for uniqueness.
- Standard building blocks first: `collections`, `itertools`, `functools`, `heapq`, `bisect`, `datetime`, `json`, `re`, before writing your own.

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

- pytest missing: `python -m pip install pytest` (into the session's venv), or use `python -m unittest`.
- Never install into the system Python, never `sudo`. An install outside the venv changes the user's machine: ask first (`permission`).
- Project's own package not found: `python -m pip install -e .`, or prefix the command with `PYTHONPATH=src` for a `src/` layout.
- CI files (`.github/workflows/*.yml`, `Makefile`) show the project's exact test command.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: `tests/test_<module>.py` with the standard `unittest` (no install), run with `python -m unittest discover -s tests`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `[0] * 18`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `assert got == want, got`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/probe.py`, run with `python tmp/probe.py`; git ignores `tmp/`.

## 3. Code that runs

Raise the type and message the request or the project's API names:

```python
raise ValueError("width must be positive")   # good
raise Exception("bad input")                 # bad: callers cannot tell it apart from other errors
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
