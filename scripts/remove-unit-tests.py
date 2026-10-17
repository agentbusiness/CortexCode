#!/usr/bin/env python3
"""Remove every unit test of this repository's own code.

Rust: each `#[cfg(test)]` item inside a crate's `src` (a test module block, an out-of-line test
module and its file, a test-only fn, use or impl). JS/TS: every `*.test.*` / `*.spec.*` file of our
plugins, UI and eval harness, and `test/` folders left empty.
Never touched: benchmark and indexer fixtures, vendored code, end-to-end tests (`tests/`, `verify/`).

Dry run by default (prints what it would remove); `--apply` removes.
"""
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APPLY = "--apply" in sys.argv
SKIP = ("eval/suites/", "/fixtures/", "/vendor/", "frame/testplugins/")


def ours():
    out = subprocess.run(["git", "ls-files", "-co", "--exclude-standard"], cwd=ROOT, capture_output=True, text=True, check=True).stdout
    return sorted({line for line in out.splitlines() if line and not any(part in "/" + line for part in SKIP)})


def block_end(lines, start):
    """Index of the line where the item starting at line `start` ends: its first `;` or the `}` that
    closes its first `{`, found by scanning Rust tokens (strings, raw strings, char literals and
    comments are skipped, so braces inside them do not count)."""
    text = "\n".join(lines[start:])
    i, depth, n = 0, 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            i = text.find("\n", i)
            i = n if i < 0 else i
            continue
        if text.startswith("/*", i):
            level, i = 1, i + 2
            while i < n and level:
                if text.startswith("/*", i):
                    level, i = level + 1, i + 2
                elif text.startswith("*/", i):
                    level, i = level - 1, i + 2
                else:
                    i += 1
            continue
        raw = re.match(r"b?r(#*)\"", text[i:i + 300]) if c in "br" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")) else None
        if raw:
            close = '"' + raw.group(1)
            j = text.find(close, i + raw.end())
            i = n if j < 0 else j + len(close)
            continue
        if c == '"' or (c == "b" and text.startswith('b"', i) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))):
            i += 2 if c == "b" else 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            i += 1
            continue
        if c == "'":
            # A char literal ('x', '\n', '\u{..}'), else a lifetime ('a).
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 3)
                i = n if j < 0 else j + 1
                continue
            if i + 2 < n and text[i + 2] == "'":
                i += 3
                continue
            i += 1
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return start + text.count("\n", 0, i)
        elif c == ";" and depth == 0:
            return start + text.count("\n", 0, i)
        i += 1
    raise ValueError(f"no end for the item at line {start + 1}")


def strip_rust(path, removed_files):
    lines = open(path, encoding="utf-8").read().split("\n")
    out, index, changed = [], 0, False
    while index < len(lines):
        if lines[index].strip() != "#[cfg(test)]":
            out.append(lines[index])
            index += 1
            continue
        # Doc comments and attributes written above the cfg go with the item.
        while out and (out[-1].strip().startswith("///") or out[-1].strip().startswith("#[")):
            out.pop()
        item = index + 1
        while item < len(lines) and lines[item].strip().startswith(("#[", "///", "//")):
            item += 1
        external = re.match(r"\s*(pub(\([^)]*\))?\s+)?mod\s+(\w+)\s*;", lines[item])
        if external:
            name = external.group(3)
            base = os.path.dirname(path) if os.path.basename(path) in ("lib.rs", "main.rs", "mod.rs") else os.path.splitext(path)[0]
            for candidate in (os.path.join(base, name + ".rs"), os.path.join(base, name, "mod.rs")):
                if os.path.exists(candidate):
                    removed_files.append(candidate)
        end = block_end(lines, item)
        # A blank line left before the removed item goes too.
        while out and out[-1].strip() == "" and (end + 1 >= len(lines) or lines[end + 1].strip() == ""):
            out.pop()
        index = end + 1
        changed = True
    if changed:
        text = "\n".join(out).rstrip("\n") + "\n"
        if APPLY:
            open(path, "w", encoding="utf-8").write(text)
    return changed


def main():
    files = ours()
    rust = [f for f in files if f.endswith(".rs") and "/src/" in "/" + f and "/tests/" not in "/" + f]
    js = [f for f in files if re.search(r"\.(test|spec)\.(ts|tsx|mjs|js)$", f)]
    removed_files, edited, failed = [], [], []
    for rel in rust:
        path = os.path.join(ROOT, rel)
        if not os.path.exists(path):
            continue
        try:
            if strip_rust(path, removed_files):
                edited.append(rel)
        except ValueError as error:
            failed.append(f"{rel}: {error}")
    removed_files += [os.path.join(ROOT, f) for f in js if os.path.exists(os.path.join(ROOT, f))]
    for path in removed_files:
        if APPLY:
            os.remove(path)
    folders = sorted({os.path.dirname(path) for path in removed_files}, reverse=True)
    emptied = []
    for folder in folders:
        if os.path.basename(folder) in ("test", "tests") and os.path.isdir(folder):
            left = [name for name in os.listdir(folder) if APPLY or os.path.join(folder, name) not in removed_files]
            if not left:
                emptied.append(folder)
                if APPLY:
                    os.rmdir(folder)
    verb = "removed" if APPLY else "would remove"
    print(f"Rust: test items {verb} from {len(edited)} files")
    for rel in edited:
        print("  edit", rel)
    print(f"Files {verb}: {len(removed_files)}")
    for path in removed_files:
        print("  rm  ", os.path.relpath(path, ROOT))
    for folder in emptied:
        print("  rmdir", os.path.relpath(folder, ROOT))
    for line in failed:
        print("  NOT HANDLED", line)
    if not APPLY:
        print("Dry run. Run again with --apply to remove.")


main()
