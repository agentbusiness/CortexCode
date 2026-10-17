JSON, YAML, TOML, CSV, XML or .env values change type, lose precision or garble in code.

Command-line JSON filtering: `skill://skills-data/jq`. Tables in pandas or polars: `skill://skills-data/dataframes`.

## 1. Look at the bytes, parse, parse again after writing

```bash
file data.csv                         # encoding guess, "with BOM", CRLF line terminators
head -c 200 data.csv | od -c | head   # \r, \t, BOM (357 273 277), quotes, separators
tail -c 50 data.csv | od -c           # a last line without \n is not counted by wc -l
```

Parse and write with a library. Never `split(",")`, a regex over YAML/JSON/XML, or string concatenation.
After writing or editing a file, parse it again:

```bash
python -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' data.json
python -c 'import yaml,sys; list(yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")))' conf.yaml
python -c 'import tomllib,sys; tomllib.load(open(sys.argv[1],"rb"))' pyproject.toml
python -c 'import xml.etree.ElementTree as E,sys; E.parse(sys.argv[1])' feed.xml
```

Keep a config file's comments and order: a targeted line edit, `ruamel.yaml` or `tomlkit`. PyYAML, `json` and `configparser` drop comments.

## 2. Encodings

- `UnicodeDecodeError`, mojibake (accented letters shown as two odd characters) or `\ufeff` at the start: look at the bytes, then try `utf-8-sig`, then `cp1252`.
- Never `errors="ignore"`: it deletes data silently.
- Repair text garbled once: `s.encode("cp1252").decode("utf-8")`. Check on a sample first.
- Always pass `encoding=` when opening text. Write UTF-8 without BOM unless Excel needs one.
- Compare text after `unicodedata.normalize("NFC", s)`: `"é"` and `"é"` look the same.

## 3. CSV

- Fields may hold quoted commas, `""` and newlines: use the CSV module with `open(p, newline="", encoding="utf-8")`.
- The writer emits `\r\n` unless `lineterminator="\n"`.
- Excel exports start with a BOM: read with `encoding="utf-8-sig"`, or the first header becomes `"\ufeffid"`.
- Semicolon files usually have decimal commas: `delimiter=";"`, then convert `"1,5"`.
- Every value is a string: convert per column. An empty string is not NULL unless you decide so.
- Leading zeros (`007`, ZIP codes, phone numbers) die in a number conversion: keep those columns as text.

## 4. YAML

Unquoted scalars change type (PyYAML follows YAML 1.1):

| Unquoted | Parses as | Write |
|---|---|---|
| `NO`, `no`, `off`, `yes`, `on`, `y` | boolean (country `NO` -> `False`) | `"NO"` |
| `1.10`, `3.0` | float `1.1`, `3.0` | `"1.10"` |
| `0755` | octal int `493` | `"0755"` |
| `12:30` | base-60 int `750` | `"12:30"` |
| `2024-01-02` | `date` (not JSON-serializable) | `"2024-01-02"` |
| `~`, `null`, empty | `None` | `"~"` |
| `off:` as a key | key `False` | `"off":` |

- Fix the data by quoting, not by patching the loaded value in code.
- Load with `yaml.safe_load`, or `safe_load_all` for several `---` documents. Never `yaml.load` on untrusted input.
- Quote values containing `: ` or ` #`, or starting with `*`, `&`, `!`, `%`, `@`, `{`, `[`, `-`, `?`.
- Double quotes process `\` escapes. Single quotes are literal (`''` for a quote).
- "found character '\t' that cannot start any token": a tab in indentation.
- Dump: `yaml.safe_dump(data, sort_keys=False, allow_unicode=True, default_flow_style=False)`.

## 5. JSON

- Python `json.dumps` writes `NaN`/`Infinity`, which other parsers reject: `allow_nan=False`.
- `Decimal`, `datetime`, `set`, bytes and numpy types do not serialize: convert them (`dt.isoformat()`). `default=str` hides mistakes.
- Exact decimals: `json.loads(s, parse_float=Decimal)`.
- Integer keys come back as strings (`{1: "x"}` -> `{"1": "x"}`). A duplicate key keeps the last value silently.
- Integers above 2^53 (ids, int64) lose precision in JavaScript: send them as strings.
- `tsconfig.json`, `.jsonc` and editor settings allow comments and trailing commas: edit them with a targeted line edit.
- JSON Lines: one value per line. Skip blank lines; report the line number of a bad one.
- Stable output: `json.dumps(obj, indent=2, sort_keys=True, ensure_ascii=False) + "\n"`; JS `JSON.stringify(obj, null, 2) + "\n"`.

| Language | Trap | Fix |
|---|---|---|
| Go | numbers in `interface{}` are `float64` | `decoder.UseNumber()` or typed structs |
| Go | lowercase fields are skipped | export them, add `json:"name"` tags |
| Go | `omitempty` drops `0`, `false`, `""` | pointer field (`*int`) |
| Go | unknown fields ignored, typos pass | `dec.DisallowUnknownFields()` |
| Rust serde | names must match exactly | `#[serde(rename_all = "camelCase")]`, `#[serde(default)]`, `Option<T>` |
| JS | `JSON.stringify` drops `undefined`, makes `Date` a string, throws on `BigInt` | convert first; revive dates on parse |
| Java Jackson | unknown property fails | `@JsonIgnoreProperties(ignoreUnknown = true)` |

## 6. TOML, XML, INI, .env

- TOML: `tomllib.load` needs `open(p, "rb")` and cannot write; write with `tomlkit` (keeps comments) or `tomli_w`.
- TOML: redefining a key or table is an error. `[[servers]]` appends to an array of tables. `'...'` is literal, `"..."` processes escapes.
- XML namespaced tags need the URI the root shows (`print(root.tag)`): `root.findall("a:entry", {"a": "<uri>"})`.
- XML `.text` is only the text before the first child: `"".join(el.itertext())`. Untrusted XML: `defusedxml`.
- `configparser`: keys are lowercased; values are strings (`getint`, `getboolean`); `%` interpolates (`interpolation=None`).
- `.env`: no spaces around `=`; quote values with spaces or `#`. Loaders differ on `export`, multi-line values and `${VAR}`. Add new keys to `.env.example`.
