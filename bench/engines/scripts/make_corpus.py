"""Build the fixed bake-off corpus (deterministic; run once, output committed under corpus/).

Every prompt is rendered with the official Qwen3.8 chat template (chat_template.jinja from
Qwen/Qwen3.8-27B, byte-identical copy in the MLX repo) so that all engines receive identical
token sequences through a raw-completion interface. Token counts use the model's own
tokenizer.json (HF `tokenizers`).

Sets (each prompt is distinct; the harness never sends the same prompt twice to one engine):
  decode.jsonl     code tasks with long answers (single-stream and concurrent decode)
  prefill2k.jsonl  ~2048-token prompts (real source files + a short task)
  prefill8k.jsonl  ~8192-token prompts
  agent.jsonl      multi-turn agent sessions: ~24k-token history, turns append 1-3k tokens of
                   tool output, the model then writes a tool call (decode capped at 300 tokens)
  quality_code.jsonl  held-out code continuation (raw text, no template), CortexCode's own
                   sources written 2026-10-14, after Qwen3.8's release (2026-08-14)
  quality_tools.jsonl agentic prompts whose answer is a Qwen XML tool call

Selection rules are mechanical (sorted file lists, a seeded RNG, token-length windows).
Usage: uv run --with jinja2 --with tokenizers python -I make_corpus.py <repo_root> <model_dir> <out_dir>
"""

import hashlib
import json
import os
import random
import re
import sys

import jinja2
from tokenizers import Tokenizer

ROOT, MODEL_DIR, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(OUT, exist_ok=True)
tok = Tokenizer.from_file(os.path.join(MODEL_DIR, "tokenizer.json"))
template_src = open(os.path.join(MODEL_DIR, "chat_template.jinja"), encoding="utf-8").read()


def raise_exception(msg):
    raise RuntimeError(msg)


env = jinja2.Environment(trim_blocks=False, lstrip_blocks=False, extensions=["jinja2.ext.loopcontrols"])
env.filters["tojson"] = lambda v, **k: json.dumps(v, ensure_ascii=False)
env.globals["raise_exception"] = raise_exception
template = env.from_string(template_src)


def render(messages, tools=None):
    return template.render(messages=messages, tools=tools, add_generation_prompt=True, enable_thinking=False)


def ntok(s):
    return len(tok.encode(s, add_special_tokens=False).ids)


def truncate_tokens(s, n):
    enc = tok.encode(s, add_special_tokens=False)
    if len(enc.ids) <= n:
        return s
    end = enc.offsets[n - 1][1]
    return s[:end]


rng = random.Random(20261014)
EXT_LANG = {".rs": "rust", ".py": "python", ".ts": "typescript", ".tsx": "tsx", ".js": "javascript", ".sh": "bash", ".bash": "bash"}


def list_sources(base, exts, exclude=()):
    files = []
    for dirpath, dirnames, filenames in os.walk(base):
        dirnames[:] = sorted(d for d in dirnames if d not in ("target", "node_modules", ".git", "vendor", "dist", "build", "cache", "__snapshots__", "tests", "fixtures") and not d.startswith("."))
        for f in sorted(filenames):
            p = os.path.join(dirpath, f)
            if os.path.splitext(f)[1] in exts and not any(x in p for x in exclude):
                try:
                    txt = open(p, encoding="utf-8").read()
                except (UnicodeDecodeError, OSError):
                    continue
                if "\x00" in txt or max((len(l) for l in txt.splitlines()), default=0) > 400:
                    continue  # minified/generated (structural rule)
                files.append((os.path.relpath(p, ROOT), txt))
    return files


# ---------- speed sources: public repos in eval/cache/src ----------
speed_files = []
for repo in ["eb-ripgrep", "rz-just", "eb-flask", "eb-httpx", "eb-werkzeug", "rz-click", "rz-more-itertools", "rz-jotai", "rz-zustand", "eb-excalidraw", "rz-bats-core", "rz-rbenv"]:
    speed_files += list_sources(os.path.join(ROOT, "eval/cache/src", repo), set(EXT_LANG), exclude=("woff2", "harfbuzz", "wasm"))
speed_files = [(p, t) for p, t in speed_files if ntok(t) >= 300]
rng.shuffle(speed_files)
print("speed source files", len(speed_files), file=sys.stderr)

DECODE_TASKS = [
    "Rewrite this code so that every function has thorough documentation comments and explicit error handling. Output the complete rewritten code.",
    "Write a comprehensive unit test suite for this code, covering normal cases, edge cases and error paths. Output only the test code.",
    "Refactor this code for readability: extract helpers, give variables clear names, and remove duplication. Output the full refactored code.",
    "Port this code to Go, preserving behaviour. Output the complete Go code.",
    "Explain step by step what this code does, then list every bug or risk you can find with a proposed fix as a code snippet.",
    "Add structured logging and input validation to this code. Output the complete updated code.",
]


def snippet(txt, lo, hi):
    """A window of source lines with lo..hi tokens (mechanical)."""
    lines = txt.splitlines(keepends=True)
    start = rng.randrange(0, max(1, len(lines) - 10))
    out, n = [], 0
    for l in lines[start:]:
        out.append(l)
        n += ntok(l)
        if n >= hi:
            break
    s = "".join(out)
    return s if ntok(s) >= lo else None


decode = []
i = 0
while len(decode) < 900:
    p, t = speed_files[i % len(speed_files)]
    i += 1
    s = snippet(t, 150, 450)
    if not s:
        continue
    lang = EXT_LANG[os.path.splitext(p)[1]]
    task = DECODE_TASKS[len(decode) % len(DECODE_TASKS)]
    msgs = [{"role": "user", "content": f"File `{p}`:\n\n```{lang}\n{s}```\n\n{task}"}]
    prompt = render(msgs)
    decode.append({"id": f"decode-{len(decode):04d}", "source": p, "messages": msgs, "prompt": prompt, "n_prompt_tokens": ntok(prompt)})


def long_prompt(target, first):
    """Concatenate whole source files until the rendered prompt reaches `target` tokens.
    Prompt k starts with its own first file, so no two prompts share a prefix (mechanical)."""
    task = "Summarize the architecture of the code above: list each type and function with a one-line description of its role."
    parts, files = [], []
    order = [first] + [(first * 37 + m) % len(speed_files) for m in range(1, len(speed_files))]
    for j in order:
        p, t = speed_files[j]
        if p in files:
            continue
        lang = EXT_LANG[os.path.splitext(p)[1]]
        parts.append(f"File `{p}`:\n```{lang}\n{t}```\n")
        files.append(p)
        body = "\n".join(parts)
        msgs = [{"role": "user", "content": body + "\n" + task}]
        n = ntok(render(msgs))
        if n >= target:
            overhead = n - ntok(body)
            body = truncate_tokens(body, target - overhead)
            msgs = [{"role": "user", "content": body + "\n" + task}]
            pr = render(msgs)
            return {"messages": msgs, "prompt": pr, "n_prompt_tokens": ntok(pr), "sources": files}


def make_long(name, target, count, start):
    out = []
    for k in range(count):
        item = long_prompt(target, start + k)
        item["id"] = f"{name}-{k:03d}"
        out.append(item)
    return out


prefill2k = make_long("prefill2k", 2048, 160, 0)
prefill8k = make_long("prefill8k", 8192, 80, 400)
for s in (prefill2k, prefill8k):
    firsts = [x["prompt"][:400] for x in s]
    assert len(set(firsts)) == len(firsts), "prefill prompts must not share a prefix"

# ---------- tools (shared by agent sessions and the tool-call quality set) ----------
TOOLS = [
    {"type": "function", "function": {"name": "read_file", "description": "Read a text file of the project.", "parameters": {"type": "object", "properties": {"path": {"type": "string", "description": "Path relative to the project root"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "search", "description": "Search the project with a regular expression; returns matching lines with paths and line numbers.", "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string", "description": "Directory or file to search, default the whole project"}}, "required": ["pattern"]}}},
    {"type": "function", "function": {"name": "edit_file", "description": "Replace an exact, unique snippet of a file with new text.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}}, "required": ["path", "old_text", "new_text"]}}},
    {"type": "function", "function": {"name": "write_file", "description": "Create or overwrite a file with the given content.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}}},
    {"type": "function", "function": {"name": "run_command", "description": "Run a shell command in the project root and return its output and exit code.", "parameters": {"type": "object", "properties": {"command": {"type": "string"}, "timeout_sec": {"type": "integer"}}, "required": ["command"]}}},
]
SYSTEM = "You are a coding agent working in a local repository. Use the tools to inspect and change the code. Prefer small, verified steps."


def tc(name, **args):
    return {"type": "function", "function": {"name": name, "arguments": args}}


# ---------- agent sessions (speed: multi-turn prefill + cache reuse) ----------
def doc_header(path, text):
    """Scripted documentation header the history's earlier edit calls insert (deterministic)."""
    lang = EXT_LANG[os.path.splitext(path)[1]]
    lead = {"rust": "//! ", "python": "# ", "bash": "# "}.get(lang, "// ")
    first = text.splitlines()[0] if text.strip() else ""
    names = re.findall(r"(?:fn|def|class|function|const|interface|type)\s+([A-Za-z_][A-Za-z0-9_]*)", text)[:6]
    body = [f"Module `{os.path.basename(path)}`.", "", "Main items: " + (", ".join(names) if names else "(none)") + ".",
            "Behaviour is unchanged; this header documents the file for reviewers."]
    return first, "\n".join(lead + l if l else lead.rstrip() for l in body) + "\n" + first


def agent_session(sid):
    """Session `sid`: its own shuffled file sequence over all speed sources, so sessions share
    no prefix beyond the system prompt and tool list (mechanical).
    History: the agent reads files; for each file it then inserts a documentation header with
    edit_file. A measured turn appends [the previous turn's scripted edit + its result + the next
    read_file + its 1-3k-token result + the user's request to document that file], and the model
    answers with an edit_file call (a few hundred tokens)."""
    files = list(speed_files)
    random.Random(1000 + sid).shuffle(files)
    first = files[0][0]
    msgs = [{"role": "system", "content": SYSTEM},
            {"role": "user", "content": f"Starting with `{first}`, review the modules one by one and add a module-level documentation header to each file that lacks one. Keep behaviour unchanged."}]
    fi = 0
    def nudge(p):
        return {"role": "user", "content": f"Add the documentation header to `{p}` now: call edit_file to replace its first line with a header comment of 8-15 lines (purpose, main items, invariants) followed by that first line."}
    def edit_pair(p, t):
        old, new = doc_header(p, t)
        return [{"role": "assistant", "content": "", "tool_calls": [tc("edit_file", path=p, old_text=old, new_text=new)]},
                {"role": "tool", "content": f"Edited {p}: 1 replacement."}]
    prev = None
    while ntok(render(msgs, TOOLS)) < 24000:
        p, t = files[fi]; fi += 1
        t = truncate_tokens(t, 3000)
        if prev:
            msgs += edit_pair(*prev)
        msgs += [{"role": "assistant", "content": "", "tool_calls": [tc("read_file", path=p)]}, {"role": "tool", "content": t}, nudge(p)]
        prev = (p, t)
    turns = []
    # turn 0: the history itself (cold)
    prompt = render(msgs, TOOLS)
    turns.append({"turn": 0, "prompt": prompt, "n_prompt_tokens": ntok(prompt), "source": prev[0], "n_messages": len(msgs)})
    while len(turns) < 6:
        p, t = files[fi]; fi += 1
        if ntok(t) < 900:
            continue  # each turn appends ~1-3k tokens (mechanical length window)
        t = truncate_tokens(t, 2700)
        msgs = msgs + edit_pair(*prev) + [{"role": "assistant", "content": "", "tool_calls": [tc("read_file", path=p)]}, {"role": "tool", "content": t}, nudge(p)]
        prev = (p, t)
        prompt = render(msgs, TOOLS)
        turns.append({"turn": len(turns), "prompt": prompt, "n_prompt_tokens": ntok(prompt), "source": p, "n_messages": len(msgs)})
    for a, b in zip(turns, turns[1:]):
        # byte-stable history: each turn's prompt minus the generation prompt is a prefix of the next
        gen = "<|im_start|>assistant\n<think>\n\n</think>\n\n"
        assert a["prompt"].endswith(gen) and b["prompt"].startswith(a["prompt"][: -len(gen)]), "history not append-only"
        b["appended_tokens"] = b["n_prompt_tokens"] - a["n_prompt_tokens"]
    turns[0]["appended_tokens"] = None
    return {"id": f"agent-{sid:02d}", "turns": turns, "messages": msgs, "tools": TOOLS}


agent = [agent_session(sid) for sid in range(32)]
assert len({a["turns"][0]["prompt"][:6000] for a in agent}) == len(agent)
if os.environ.get("CORPUS_ONLY") == "agent":
    # rebuild only the agent sessions; the other sets stay byte-identical (held-out sources move)
    with open(os.path.join(OUT, "agent.jsonl"), "w", encoding="utf-8") as f:
        for r in agent:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    man = json.load(open(os.path.join(OUT, "MANIFEST.json")))
    man["agent.jsonl"] = hashlib.sha256(open(os.path.join(OUT, "agent.jsonl"), "rb").read()).hexdigest()
    json.dump(man, open(os.path.join(OUT, "MANIFEST.json"), "w"), indent=1)
    for a in agent:
        print(a["id"], [(t["n_prompt_tokens"], t["appended_tokens"]) for t in a["turns"]], file=sys.stderr)
    sys.exit(0)


# ---------- quality: held-out code (CortexCode sources, written 2026-10-14) ----------
held = []
for d in ["frame/src", "protocol/src", "plugins/agents/src", "plugins/chatflow/src", "plugins/memory-irongraph/src", "plugins/model-llama/src", "plugins/tools/src", "plugins/vcs-git/src", "plugins/terminal/src", "cli/src"]:
    held += list_sources(os.path.join(ROOT, d), {".rs"})
held = [(p, t) for p, t in held if ntok(t) >= 900]
rng.shuffle(held)
quality_code = []
for p, t in held:
    if len(quality_code) == 50:
        break
    n = ntok(t)
    cut = rng.randint(500, min(1500, n - 300))
    prefix = truncate_tokens(t, cut)
    # cut at a line end so the continuation starts on a fresh line (mechanical)
    prefix = prefix[: prefix.rfind("\n") + 1]
    if ntok(prefix) < 300:
        continue
    quality_code.append({"id": f"qcode-{len(quality_code):02d}", "source": p, "sha256_source": hashlib.sha256(t.encode()).hexdigest(), "prompt": prefix, "n_prompt_tokens": ntok(prefix)})
assert len(quality_code) == 50

# ---------- quality: tool calls (agentic prompts; the expected action is a tool call) ----------
quality_tools = []
tool_files = held[:]
rng.shuffle(tool_files)
import re
fn_re = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)", re.M)
for p, t in tool_files:
    if len(quality_tools) == 40:
        break
    fns = fn_re.findall(t)
    if len(fns) < 2:
        continue
    f1, f2 = fns[0], fns[-1]
    shown = truncate_tokens(t, 1500)
    k = len(quality_tools) % 5
    if k == 0:
        user = f"Where is the function `{f2}` called from in the project? Find all call sites."
        msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}]
    elif k == 1:
        user = f"Show me lines 1 to 60 of `{p}`."
        msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}]
    elif k == 2:
        user = f"Run the tests of the crate that contains `{p}` (it is a Cargo workspace rooted at `{p.split('/src/')[0]}`)."
        msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}]
    elif k == 3:
        user = f"Rename the function `{f1}` to `{f1}_impl` at its definition in `{p}` (only the definition line for now)."
        msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user},
                {"role": "assistant", "content": "", "tool_calls": [tc("read_file", path=p)]},
                {"role": "tool", "content": shown}]
    else:
        user = f"Create a file `{p.rsplit('/', 1)[0]}/NOTES.md` that lists every function defined in `{p}` with a one-line description of each."
        msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user},
                {"role": "assistant", "content": "", "tool_calls": [tc("read_file", path=p)]},
                {"role": "tool", "content": shown}]
    prompt = render(msgs, TOOLS)
    quality_tools.append({"id": f"qtool-{len(quality_tools):02d}", "kind": ["search", "read_file", "run_command", "edit_file", "write_file"][k], "source": p, "messages": msgs, "tools": TOOLS, "prompt": prompt, "n_prompt_tokens": ntok(prompt)})
assert len(quality_tools) == 40


def dump(name, rows):
    with open(os.path.join(OUT, name), "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


dump("decode.jsonl", decode)
dump("prefill2k.jsonl", prefill2k)
dump("prefill8k.jsonl", prefill8k)
dump("agent.jsonl", agent)
dump("quality_code.jsonl", quality_code)
dump("quality_tools.jsonl", quality_tools)
json.dump({"tools": TOOLS}, open(os.path.join(OUT, "tools.json"), "w"), indent=1)
manifest = {}
for n in sorted(os.listdir(OUT)):
    if n.endswith(".jsonl") or n == "tools.json":
        manifest[n] = hashlib.sha256(open(os.path.join(OUT, n), "rb").read()).hexdigest()
manifest["template_sha256"] = hashlib.sha256(template_src.encode()).hexdigest()
manifest["tokenizer_sha256"] = hashlib.sha256(open(os.path.join(MODEL_DIR, "tokenizer.json"), "rb").read()).hexdigest()
json.dump(manifest, open(os.path.join(OUT, "MANIFEST.json"), "w"), indent=1)
for n, rows in [("decode", decode), ("prefill2k", prefill2k), ("prefill8k", prefill8k), ("quality_code", quality_code), ("quality_tools", quality_tools)]:
    ns = [r["n_prompt_tokens"] for r in rows]
    print(n, len(rows), "tokens min/max", min(ns), max(ns), file=sys.stderr)
for a in agent:
    print(a["id"], [(t["n_prompt_tokens"], t["appended_tokens"]) for t in a["turns"]], file=sys.stderr)
