Code passing outside input into shell, SQL, file paths, HTML or deserializers, or handling secrets.

Never concatenate outside data (request fields, headers, uploads, CLI arguments, user rows, model output) into anything interpreted. Use the safe form:

| Sink | Wrong | Right |
|---|---|---|
| shell | `os.system(f"convert {name}")`, `shell=True`, `exec("ls " + d)` | argument list: `subprocess.run(["convert", "--", name], check=True)`, `execFile`, `exec.Command`; `--` stops `-rf` acting as an option |
| SQL | f-strings, `+`, `%` | placeholders: `cur.execute("SELECT * FROM t WHERE id = ?", (id,))`; table and column names from an allowlist |
| paths | `open(base + "/" + name)` | a generated name, or the containment check below |
| HTML | `innerHTML`, a template `safe` filter, `dangerouslySetInnerHTML`, `v-html` | `textContent`, auto-escaping templates |
| deserialize | `pickle.loads`, `yaml.load`, `ObjectInputStream`, `unserialize`, `eval` | JSON or `yaml.safe_load`, then validate |
| regex | user text in a pattern | `re.escape` or equivalent |
| outbound URL, redirect | any URL a user gives | allowlisted schemes and hosts; no private or loopback addresses; redirects to relative paths only |

## File names and paths

- Store uploads under a generated name (`uuid4().hex`) plus an allowlisted extension; the client's name is data only.
- A user path you must use: take its last component, reject empty, `.` and `..`, check containment:

```python
root = Path(base).resolve()
target = (root / user_path).resolve()
if not target.is_relative_to(root):
    raise ValueError("path escapes the base directory")
```

- Node: `path.resolve(root, p)` must start with `root + path.sep`. Go: `filepath.Rel(root, p)` must not start with `..`.
- Archives: check every member first. Python `tarfile`: `extractall(filter="data")`.

## Secrets

- No keys, tokens or passwords in code, tests, fixtures, logs, errors or URLs. Read them from the environment; fail clearly if missing.
- Before committing: the secret scan in `skill://skills-engineering/git-and-github`.
- A committed secret stays in history: report it.

## Auth and crypto

- Each handler checks access to the object (this user owns record 42), not just "logged in".
- Never trust a client's role, user id or price. Copy only allowed request-body fields.
- Passwords: argon2, bcrypt or scrypt. Tokens: `secrets`, `crypto.randomBytes`, `crypto/rand`, `SecureRandom`; never `random` or `Math.random`.
- Compare secrets with `hmac.compare_digest`, `crypto.timingSafeEqual`, `subtle.ConstantTimeCompare`.
- `verify=False`, `rejectUnauthorized: false`, `InsecureSkipVerify: true` are bugs.
- Temp files: `mkstemp` or `TemporaryDirectory`, not fixed `/tmp` names.
- Limit upload and page size; time out outbound calls.

## Fixing a vulnerability

1. Write a test with hostile input; it must fail now: `../../etc/passwd`, `-rf`, `x' OR '1'='1`, `$(id)`, `<script>alert(1)</script>`.
2. Fix at the sink with the safe form, not by filtering characters at entry.
3. Fix the same pattern elsewhere:

```bash
git grep -nE 'shell=True|os\.system|innerHTML|pickle\.loads|yaml\.load\('
```

4. The hostile test and module suite pass.
