A regex gives wrong matches, hangs on some input or differs across engines, grep or sed; not globs.

## 1. Test a pattern before using it

1. Write cases: strings that must match (with captures) and strings that must not.
2. Include: empty, near miss, match inside other text, unicode, trailing newline.
3. Run them in one command, in the engine that will run the pattern:

```bash
python3 - <<'EOF'
import re
p = re.compile(r"([0-9]{4})-([0-9]{2})-([0-9]{2})")
for s, want in [("2024-05-01", ("2024", "05", "01")), ("2024-5-1", None), ("2024-05-01\n", None), ("x2024-05-01", None)]:
    m = p.fullmatch(s)
    got = m.groups() if m else None
    print("ok " if got == want else "BAD", repr(s), got)
EOF
```

## 2. Common bugs (bad -> good)

```text
".*"        first quote to the LAST quote on the line    ->  "[^"]*"   (with escapes: "(?:[^"\\]|\\.)*")
<.+>        whole line of tags                           ->  <[^>]+>   (HTML: use a parser)
[A-z]       also matches [ \ ] ^ _ `                     ->  [A-Za-z]
in|int      on "int" matches "in"                        ->  \b(?:int|in)\b
a.b         built from user input                        ->  re.escape(user_text)
\d+         Python also matches non-ASCII digits         ->  [0-9]+
[0-255]     a character class, not a number range        ->  25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9]
\.(jpg|png) also matches "x.jpgfoo"                      ->  \.(?:jpg|png)$
```

- Between delimiters use a negated class (`[^,]*`), not `.*?`: it cannot run past the delimiter.
- URLs, emails, dates, IPs, JSON, HTML, CSV: use a parser (`urllib.parse`, `datetime.strptime`, `ipaddress.ip_address`).

## 3. Hangs on one input: catastrophic backtracking

Nested or overlapping quantifiers fail exponentially on input that almost matches: `(a+)+$`, `(\w+\s?)*$`, `(.*,)*`, `(a|aa)+`, `(\d+)*x`.

1. The bad input usually does NOT match: a truncated line, a missing delimiter, a very long line.
2. Time it; each extra character doubles the time:
   ```bash
   python -c 'import re,time,sys; t=time.time(); re.search(sys.argv[1], sys.argv[2]); print(time.time()-t)' '(\w+\s?)*$' 'word word word word word word!'
   ```
3. Make every character matchable in only one way: `(a+)+` -> `a+`; `(\w+\s?)*` -> `\w+(?:\s\w+)*`; `.*` between delimiters -> `[^,]*`.
4. Or atomic `(?>...)` / possessive `a++` (Python 3.11+, Java, PCRE, .NET). Untrusted input: Go, Rust or RE2, or limit the length.
5. Rerun on the bad input and on normal ones.

## 4. Engine differences

| | Python `re` | JavaScript | Go `regexp` | Rust `regex` | Java / PCRE |
|---|---|---|---|---|---|
| Lookaround | yes; lookbehind fixed width | yes | **no** | **no** | yes (Java: bounded) |
| Backreference `\1` | yes | yes | **no** | **no** | yes |
| Named group | `(?P<n>...)` | `(?<n>...)` | `(?P<n>...)`; `(?<n>...)` 1.22+ | both | `(?<n>...)` |
| `\d`, `\w`, `\b` | Unicode (`re.ASCII` to limit) | ASCII | ASCII | Unicode | ASCII (Java `(?U)` for Unicode) |
| `$` without multiline | end, or before a final `\n` | end | end | end | end, or before a final `\n` |
| Whole-string match | `re.fullmatch` | `^...$` | `^...$` | `^...$` | Java `matches()` |
| Replacement group | `\1`, `\g<n>` | `$1`, `$<n>` | `${1}` | `${1}` | `$1` |
| Exponential backtracking | yes | yes | no | no | yes |

- Go, Rust: no lookaround or backreferences; capture more and check in code.
- String literals: Python `r"\d+"`, Go `` `\d+` ``, Rust `r"\d+"`.
- Java and C# double every backslash: `"\\d+"`, `"\\\\"` for one literal backslash.
- JS: `/\d+/`, but `new RegExp("\\d+")`.

## 5. Anchors and flags

- `re.search(r"abc$", "abc\n")` matches in Python and PCRE. Validate with `re.fullmatch`, or end with `\Z` (Python) / `\z` (PCRE, Java, Rust, Go).
- `re.match` anchors only the start.
- Go `MatchString` and Rust `is_match` search anywhere: anchor explicitly.
- `^`/`$` per line need multiline: `re.M`, `/m`, `(?m)`.
- `.` matches newline only with `re.S`, `/s`, `(?s)`.

## 6. Language traps

Python:
- `re.findall` returns group tuples when the pattern has groups: use `(?:...)`, or `finditer` with `m.group(0)`.
- `re.split` with a capture group keeps the separators.
- Replacement strings interpret backslashes: pass `lambda m: text` for literal text.
- `re.sub(p, r, s, re.I)` passes `re.I` as `count`: write `flags=re.I`.

JavaScript:
- A `/g` or `/y` regex keeps `lastIndex`: repeated `re.test(s)` alternates true and false.
- `str.replace("a", ...)` replaces the first match only: `replaceAll` or `/a/g`.
- `matchAll` needs `g`; `\p{L}` needs `u`.

Go: `regexp.MustCompile` at package level; `FindAllStringSubmatch(s, -1)`. In a replacement `$1a` names group `1a`: write `${1}a`.

Rust: compile once (`std::sync::LazyLock<Regex>`); `captures_iter` for all matches.

Java: `String.matches` anchors the whole string. `split` and `replaceAll` take a regex: `"a.b".split(".")` is empty, write `split("\\.")`. `replace` is literal.

## 7. grep, sed, ripgrep

- `grep` and `sed` default to basic regex: `+ ? | ( ) { }` are literal unless escaped. Use `-E` and write them unescaped.
- ERE has no `\d`, no lazy `*?`, no lookaround. Use `[0-9]`, `[[:alnum:]_]`, `[[:space:]]`. `grep -w` matches whole words.
- `grep -P` is GNU only (missing on macOS). `rg` has no lookaround; `rg -P` adds lookaround and backreferences.
- Literal text: `grep -F`, `rg -F`. A pattern starting with `-` needs `--` before it.
- Only the match: `grep -oE`, `rg -o`. One group: `rg -o -r '$1' 'id=([0-9]+)'`, or `sed -nE 's/.*id=([0-9]+).*/\1/p'`.
- In-place sed: GNU `sed -i 's/a/b/'`, macOS `sed -i '' 's/a/b/'`. `sed -i.bak` works on both (delete the `.bak` files after).
- In a sed replacement escape `&`, `\` and the delimiter, or switch delimiter: `s|/old|/new|`.
- Single-quote patterns in the shell. A single quote inside: `'it'"'"'s'`, or a pattern file (`grep -f pat.txt`).
