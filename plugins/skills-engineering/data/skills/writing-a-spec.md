A long or vague request to turn into checkable requirements, or the session's spec; not a plan.

## How the session keeps a spec

- The host builds it from a long request and amends it from later messages.
- Shown first in the conversation: `Spec, version N: <title>`, text, `Requirements (id [status] text)`, accepted decisions.
- Ids `R1`, `R2` ...; status `open`, `met` or `dropped`.
- Refer to requirements by id; never restate the spec in chat.

## 1. Write requirements

One observable behaviour per requirement, one sentence, one of these forms:

| Kind | Form |
|---|---|
| Always | The <component> shall <behaviour>. |
| Event | When <trigger>, the <component> shall <response>. |
| State | While <state>, the <component> shall <behaviour>. |
| Unwanted | If <bad condition>, then the <component> shall <response>. |
| Option | Where <feature or setting> is enabled, the <component> shall <behaviour>. |

- Split each "and" joining two behaviours.
- Vague word -> a bound: "fast" -> "within 200 ms for 10 000 rows". No number given: ask; never invent one.
- What, not how: behaviour at the boundary (CLI output, API response, file written). A technology the user demands is a constraint; one you pick belongs in the plan.
- Non-goals are requirements too: "The export shall not change the public API of `Store`."
- Unwanted cases implied but not listed (empty input, missing file, offline, too large, interrupted): "Proposed" in your reply; spec only once accepted.

Bad: "Export should be robust and fast, with CSV and JSON."
Good:
- R1 When the user runs `app export --format csv <file>`, the CLI shall write all rows as CSV with a header row.
- R2 When `--format json` is given, the CLI shall write a JSON array of row objects.
- R3 If the target file exists and `--force` is not given, then the CLI shall exit with status 1 and leave the file unchanged.

## 2. Ask

At most three questions, each with your default: "R3: overwrite an existing file? Default: refuse unless --force."

## 3. Give each requirement a check

- `test: <command running one named test>`: Given (fixture), When (the call), Then (assertions on values from the user's text, never your own calculation).
- `run: <command>` with the expected output the user gave.
- `manual: <what the user looks at>`, only if no command can show it.

Put them in the spec's text or your reply. Only a check you ran covers a requirement, not written code: `R2 covered: test export::json_array passes`.

## Writing the session's spec yourself

Only when the user asks.
- `requirements` is the whole new list: a missing id is dropped; an entry without `id` is new.
- The heredoc is unquoted, so `$CORTEX_SESSION_ID` (this session) expands.
- In strings write `\"`, `\n`, `\$`, `` \` `` for a quote, line break, `$`, backtick.

```bash
cat > /tmp/spec.json <<EOF
{"session_id": "$CORTEX_SESSION_ID", "title": "CSV and JSON export",
 "text": "<the request as agreed, then one acceptance line per requirement id>",
 "requirements": [{"id": "R1", "text": "When ..."}, {"text": "If ..."}]}
EOF
cx call chatflow.spec.update @/tmp/spec.json
cx call chatflow.spec.save "{\"session_id\": \"$CORTEX_SESSION_ID\"}"   # writes docs/specs/<slug>.md; only when the user wants it in the repo
```
