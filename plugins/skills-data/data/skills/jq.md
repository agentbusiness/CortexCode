Filtering, reshaping, building or extracting JSON on the command line with jq; not JSON in code.

Check `jq --version`: 1.6 rounds large integers even when it only reads them; `pick` needs 1.7.

## 1. Look at the shape, then write the filter

```bash
jq 'type, (if type == "object" then keys else length end)' f.json
jq '.items[0]' f.json
jq '[.items[] | keys] | add | unique' f.json      # every key any item uses
jq -c 'paths' f.json | head -30                    # the shape, one path per line
```

Filter prints nothing: run it in pieces (`.items`, `.items[]`, `.items[] | select(...)`) to see where the values disappear.

## 2. Options

| Option | Effect |
|---|---|
| `-r` | raw strings, no quotes |
| `-c` | one compact value per line |
| `-e` | exit 1 when the last output is `null` or `false`, 4 when there is none; use it in checks |
| `-s` | slurp all inputs into one array |
| `-n` | no input; read with `inputs`, or build JSON from arguments |
| `-R` | input lines as strings (with `-s`: the whole file as one string) |
| `--arg n "$x"` | `$n` is always a string |
| `--argjson n 5` | `$n` is parsed JSON |

- Never splice shell variables into the filter text: pass them with `--arg`/`--argjson`.
- Environment variables: `$ENV.HOME`.
- jq has no in-place edit:

```bash
jq '.version = $v' --arg v "$NEW" package.json > package.json.tmp && mv package.json.tmp package.json
```

## 3. Patterns

```bash
jq -r '.users[] | select(.plan.tier == "pro") | .id' f.json     # one raw value per line
jq -r '.items[] | [.id, .name] | @tsv' f.json                   # table out (@csv quotes strings)
jq '.items | map(.price) | add // 0' f.json                     # sum, 0 for an empty array
jq '.items | group_by(.kind) | map({kind: .[0].kind, n: length, total: (map(.v) | add)})' f.json
jq '.items | INDEX(.id)' f.json                                 # array -> object keyed by id
jq 'to_entries | map(select(.value > 1)) | from_entries' f.json # filter an object by value
jq -s '.[0] * .[1]' base.json override.json                     # deep merge
jq '.. | objects | select(has("error"))' f.json                 # search at any depth
jq 'del(.items[] | select(.tmp))' f.json                        # delete matching elements
jq 'walk(if type == "object" then del(.password) else . end)' f.json   # remove a key everywhere
jq '.items[] |= (.price *= 1.2)' f.json                         # update inside the document
jq '.payload | fromjson | .user' f.json                         # JSON embedded in a string
jq -r '.items[] | select(.name | test("^api-"; "i")) | .name' f.json
jq -r '.items[] | "\(.id)\t\(.name // "-")"' f.json            # string interpolation
```

Build JSON from shell values (quotes, newlines and unicode are escaped for you):

```bash
jq -n --arg name "$NAME" --argjson port "$PORT" '{name: $name, port: $port}'
```

Into the shell: `jq -r '.files[] | @sh'` quotes each value. Loop over fields:

```bash
jq -r '.items[] | [.id, .url] | @tsv' f.json | while IFS=$'\t' read -r id url; do echo "$id $url"; done
```

Lines of text to JSON strings: `jq -R . lines.txt`.

## 4. JSON Lines and large input

- On `.jsonl`, jq runs the filter once per line. Add `-s` only when you need all lines at once.
- Aggregate without loading everything: `jq -n '[inputs | .ms] | add / length' big.jsonl`, or `reduce inputs as $r (0; . + $r.ms)`.
- A bad line stops jq at `parse error ... at line N`. Skip bad lines: `jq -R 'fromjson? // empty' f.jsonl`.

## 5. Traps

| Symptom | Cause | Fix |
|---|---|---|
| `select(.id == "1")` matches nothing | number vs string | `.id == 1`, or `tostring`/`tonumber` |
| `Cannot iterate over null` | `.tags[]` on a missing key | `.tags[]?` or `(.tags // [])[]` |
| `Cannot index array with "x"` | `.x` on an array | `.[].x` or `map(.x)` |
| `Cannot index string with "x"` | JSON inside a string | `fromjson` first |
| `.flag // true` is true for `false` | `//` treats `false` like `null` | `if .flag == null then true else .flag end` |
| big ids change (`...567000`) | numbers become doubles on arithmetic (1.6: on any read) | keep ids as strings |
| several outputs where one was expected | `.[]` or `..` produced a stream | wrap in `[...]`, or `first(...)` |
| `@csv` / `@tsv` error | needs an array of scalars | build `[.a, .b]` first |
| `$x` is a string where a number was needed | `--arg` | `--argjson`, or `($x | tonumber)` |
| syntax error near a shell `$var` | double-quoted filter: the shell expanded `$` | single-quote the filter, pass values with `--arg` |
| `.my-key` fails | parsed as subtraction | `."my-key"` or `.["my-key"]` |

## 6. YAML with the same filters

`yq` is two incompatible tools. Check `yq --version`.
- Go yq: `yq -i '.spec.replicas = 3' deploy.yaml` (in place, keeps most comments).
- Python yq wraps jq: `yq -y '.spec.replicas = 3' deploy.yaml` (drops comments).
- Neither installed: convert in Python (`yaml.safe_load` -> `json.dumps`) and use jq, or edit the YAML directly.
