"""Structural mutation engine for the edit-format benchmark (Stencil methodology).

Reads a JSON request on stdin and writes JSON to stdout.

  {"op": "candidates", "lang": "python|typescript|tsx|rust", "source": "..."}
     -> {"ok": true, "parse_errors": bool, "functions": [...], "candidates": [...]}
  {"op": "check", "lang": ..., "source": "..."}
     -> {"ok": true, "parse_errors": bool}

Every rule here is structural (tree-sitter node types, token text, byte ranges). Nothing judges
meaning. Each candidate is a reversible byte-range replacement: applying `replacement` at
[start, end) yields the mutated file; the original bytes are kept so the mutation can be inverted.
"""

import json
import os
import sys

import tree_sitter_language_pack as tslp
from tree_sitter import Parser

# Grammars are cached under eval/cache/toolchains so mutation works offline once prepared.
if os.environ.get("CCEVAL_TS_CACHE"):
    tslp.configure({"cache_dir": os.environ["CCEVAL_TS_CACHE"]})

# Operator swap pairs (structural token rewrite) and the operator class named in the task text.
SWAP = {
    "===": ("!==", "equality"), "!==": ("===", "equality"),
    "==": ("!=", "equality"), "!=": ("==", "equality"),
    "<": (">", "comparison"), ">": ("<", "comparison"),
    "<=": (">=", "comparison"), ">=": ("<=", "comparison"),
    "&&": ("||", "logical"), "||": ("&&", "logical"),
    "and": ("or", "logical"), "or": ("and", "logical"),
    "+": ("-", "arithmetic"), "-": ("+", "arithmetic"),
    "*": ("/", "arithmetic"), "/": ("*", "arithmetic"),
}

FUNC_TYPES = {
    "python": {"function_definition"},
    "typescript": {"function_declaration", "method_definition", "generator_function_declaration"},
    "tsx": {"function_declaration", "method_definition", "generator_function_declaration"},
    "rust": {"function_item"},
}
ARROW_TYPES = {"arrow_function", "function_expression", "function"}
BINARY_TYPES = {
    "python": {"binary_operator", "comparison_operator", "boolean_operator"},
    "typescript": {"binary_expression"},
    "tsx": {"binary_expression"},
    "rust": {"binary_expression"},
}
EXIT_TYPES = {
    "python": {"return_statement": "return", "raise_statement": "raise",
               "continue_statement": "continue", "break_statement": "break"},
    "typescript": {"return_statement": "return", "throw_statement": "throw",
                   "continue_statement": "continue", "break_statement": "break"},
    "rust": {"return_expression": "return", "continue_expression": "continue",
             "break_expression": "break"},
}
EXIT_TYPES["tsx"] = EXIT_TYPES["typescript"]


def parser_for(lang):
    p = Parser()
    p.language = tslp.get_language(lang)
    return p


def has_errors(node):
    if node.type == "ERROR" or node.is_missing:
        return True
    if not node.has_error:
        return False
    return any(has_errors(c) for c in node.children)


def walk(node):
    stack = [node]
    while stack:
        n = stack.pop()
        yield n
        stack.extend(reversed(n.children))


def text(src, node):
    return src[node.start_byte:node.end_byte].decode("utf8", "replace")


def func_name(lang, src, node):
    """Name of a function-like node, or None (structural: name field / declarator name)."""
    if node.type in FUNC_TYPES[lang]:
        name = node.child_by_field_name("name")
        return text(src, name) if name is not None else None
    if lang != "python" and lang != "rust" and node.type in ARROW_TYPES:
        parent = node.parent
        if parent is not None and parent.type == "variable_declarator":
            name = parent.child_by_field_name("name")
            if name is not None and name.type == "identifier":
                return text(src, name)
        if parent is not None and parent.type in ("pair", "public_field_definition", "field_definition"):
            key = parent.child_by_field_name("key") or parent.child_by_field_name("name")
            if key is not None and key.type in ("property_identifier", "identifier"):
                return text(src, key)
    return None


def enclosing_function(lang, src, node):
    """Innermost named function containing `node` -> (name, function node)."""
    cur = node.parent
    while cur is not None:
        name = func_name(lang, src, cur)
        if name:
            return name, cur
        cur = cur.parent
    return None, None


def in_test_module(lang, src, node):
    """Rust unit tests live in `mod tests`; mutations target product code only (structural)."""
    if lang != "rust":
        return False
    cur = node.parent
    while cur is not None:
        if cur.type == "mod_item":
            name = cur.child_by_field_name("name")
            if name is not None and text(src, name) in ("tests", "test"):
                return True
        cur = cur.parent
    return False


def receiver_name(lang, src, fn):
    """Python method receiver (first parameter of a function defined directly in a class body)."""
    if lang != "python":
        return None
    parent = fn.parent
    if parent is not None and parent.type == "decorated_definition":
        parent = parent.parent
    if parent is None or parent.type != "block" or parent.parent is None or parent.parent.type != "class_definition":
        return None
    params = fn.child_by_field_name("parameters")
    if params is None or not params.named_children:
        return None
    first = params.named_children[0]
    return text(src, first) if first.type == "identifier" else None


def function_body(lang, fn):
    return fn.child_by_field_name("body")


def line_span(src, node):
    """Byte range of the full lines the node occupies, if it occupies them alone."""
    start = node.start_byte
    end = node.end_byte
    ls = src.rfind(b"\n", 0, start) + 1
    if src[ls:start].strip():
        return None
    le = src.find(b"\n", end)
    le = len(src) if le < 0 else le + 1
    if src[end:le].strip() not in (b"", b";"):
        return None
    return ls, le


def guard_exit(lang, src, stmt):
    """If `stmt` is a guard clause (if without else whose body is one early exit), the exit kind."""
    exits = EXIT_TYPES[lang]
    if lang == "rust":
        if stmt.type == "expression_statement" and stmt.named_child_count == 1:
            stmt = stmt.named_children[0]
        if stmt.type != "if_expression" or stmt.child_by_field_name("alternative") is not None:
            return None
        body = stmt.child_by_field_name("consequence")
        inner = [c for c in body.named_children if c.type not in ("line_comment", "block_comment")]
        if len(inner) != 1:
            return None
        e = inner[0]
        if e.type == "expression_statement" and e.named_child_count == 1:
            e = e.named_children[0]
        return exits.get(e.type)
    if stmt.type != "if_statement" or stmt.child_by_field_name("alternative") is not None:
        return None
    body = stmt.child_by_field_name("consequence")
    if body is None:
        return None
    if body.type in ("statement_block", "block"):
        inner = [c for c in body.named_children if c.type != "comment"]
        if len(inner) != 1:
            return None
        e = inner[0]
    else:
        e = body
    return exits.get(e.type)


def declared_names(lang, src, fn):
    """Local names declared in a function: parameters and simple variable bindings (structural)."""
    names = set()
    for n in walk(fn):
        if lang == "python":
            if n.type in ("parameters", "lambda_parameters"):
                for c in n.named_children:
                    if c.type == "identifier":
                        names.add(text(src, c))
                    elif c.type in ("default_parameter", "typed_parameter", "typed_default_parameter"):
                        nm = c.child_by_field_name("name") or (c.named_children[0] if c.named_children else None)
                        if nm is not None and nm.type == "identifier":
                            names.add(text(src, nm))
            elif n.type == "assignment":
                left = n.child_by_field_name("left")
                if left is not None and left.type == "identifier":
                    names.add(text(src, left))
            elif n.type == "for_statement":
                left = n.child_by_field_name("left")
                if left is not None and left.type == "identifier":
                    names.add(text(src, left))
        elif lang == "rust":
            if n.type in ("let_declaration", "parameter"):
                pat = n.child_by_field_name("pattern")
                if pat is not None and pat.type == "identifier":
                    names.add(text(src, pat))
        else:
            if n.type == "variable_declarator":
                nm = n.child_by_field_name("name")
                if nm is not None and nm.type == "identifier":
                    names.add(text(src, nm))
            elif n.type in ("required_parameter", "optional_parameter"):
                pat = n.child_by_field_name("pattern")
                if pat is not None and pat.type == "identifier":
                    names.add(text(src, pat))
    return names


def is_declaration_site(lang, node):
    p = node.parent
    if p is None:
        return True
    if lang == "python":
        if p.type in ("parameters", "lambda_parameters", "default_parameter", "typed_parameter",
                      "typed_default_parameter", "keyword_argument", "attribute"):
            if p.type == "attribute" and p.child_by_field_name("object") == node:
                return False
            if p.type == "keyword_argument" and p.child_by_field_name("value") == node:
                return False
            return True
        if p.type == "assignment" and p.child_by_field_name("left") == node:
            return True
        if p.type == "for_statement" and p.child_by_field_name("left") == node:
            return True
        return False
    if lang == "rust":
        if p.type in ("let_declaration", "parameter") and p.child_by_field_name("pattern") == node:
            return True
        return p.type in ("field_expression",) and p.child_by_field_name("field") == node
    if p.type == "variable_declarator" and p.child_by_field_name("name") == node:
        return True
    if p.type in ("required_parameter", "optional_parameter") and p.child_by_field_name("pattern") == node:
        return True
    if p.type in ("shorthand_property_identifier", "shorthand_property_identifier_pattern", "pair_pattern"):
        return True
    return False


def transpose_name(name):
    """Deterministic misspelling: swap the two characters around the middle that differ."""
    mid = len(name) // 2
    order = sorted(range(1, len(name) - 1), key=lambda i: (abs(i - mid), i))
    for i in order:
        if i + 1 < len(name) and name[i] != name[i + 1] and name[i].isalnum() and name[i + 1].isalnum():
            return name[:i] + name[i + 1] + name[i] + name[i + 2:]
    return None


def candidates(lang, src):
    tree = parser_for(lang).parse(src)
    root = tree.root_node
    out = []
    all_identifiers = set()
    functions = {}
    for n in walk(root):
        if n.type in ("identifier", "property_identifier", "field_identifier", "type_identifier",
                      "shorthand_property_identifier"):
            all_identifiers.add(text(src, n))
        nm = func_name(lang, src, n)
        if nm:
            functions.setdefault(nm, []).append(n)
    unique_funcs = {k for k, v in functions.items() if len(v) == 1}

    def add(kind, node, start, end, replacement, extra):
        fname, fnode = enclosing_function(lang, src, node)
        if not fname or fname not in unique_funcs or in_test_module(lang, src, node):
            return
        line = src.count(b"\n", 0, start) + 1
        out.append({
            "type": kind, "start": start, "end": end,
            "original": src[start:end].decode("utf8"),
            "replacement": replacement, "function": fname, "line": line, **extra,
        })

    for n in walk(root):
        # operator_swap
        if n.type in BINARY_TYPES[lang]:
            op = n.child_by_field_name("operator")
            ops = [op] if op is not None else [c for c in n.children if not c.is_named]
            if n.type == "comparison_operator":
                ops = [c for c in n.children if not c.is_named]
                if len(ops) != 1:
                    ops = []
            for o in ops[:1]:
                tok = text(src, o)
                if tok in SWAP:
                    rep, cls = SWAP[tok]
                    if lang == "python" and tok in ("&&", "||", "===", "!=="):
                        continue
                    if lang != "python" and tok in ("and", "or"):
                        continue
                    add("operator_swap", n, o.start_byte, o.end_byte, rep, {"operator_class": cls})
        # boolean_flip
        if n.type in ("true", "false") or (lang == "rust" and n.type == "boolean_literal"):
            t = text(src, n)
            flip = {"true": "false", "false": "true", "True": "False", "False": "True"}.get(t)
            if flip:
                add("boolean_flip", n, n.start_byte, n.end_byte, flip, {})
        # off_by_one: decimal integer literal used in a comparison, arithmetic, index or range
        if n.type in ("number", "integer", "integer_literal"):
            t = text(src, n)
            parent = n.parent
            ptypes = BINARY_TYPES[lang] | {"subscript", "subscript_expression", "index_expression",
                                            "range_expression", "slice"}
            if t.isdigit() and parent is not None and parent.type in ptypes and len(t) < 6:
                v = int(t)
                rep = str(v + 1) if (v == 0 or (n.start_byte % 2 == 0)) else str(v - 1)
                add("off_by_one", n, n.start_byte, n.end_byte, rep, {})
        # removed_optional_chain (TS/TSX only)
        if lang in ("typescript", "tsx") and n.type == "optional_chain":
            after = src[n.end_byte:n.end_byte + 1]
            rep = "" if after in (b"[", b"(") else "."
            add("removed_optional_chain", n, n.start_byte, n.end_byte, rep, {})
    # removed_guard_clause: statements directly in a named function's body
    for name in unique_funcs:
        fn = functions[name][0]
        body = function_body(lang, fn)
        if body is None:
            continue
        if in_test_module(lang, src, fn):
            continue
        for stmt in body.named_children:
            kind = guard_exit(lang, src, stmt)
            if not kind:
                continue
            span = line_span(src, stmt)
            if not span:
                continue
            fname, _ = enclosing_function(lang, src, stmt)
            if fname != name:
                continue
            line = src.count(b"\n", 0, span[0]) + 1
            out.append({
                "type": "removed_guard_clause", "start": span[0], "end": span[1],
                "original": src[span[0]:span[1]].decode("utf8"), "replacement": "",
                "function": name, "line": line, "exit_kind": kind,
            })
    # renamed_identifier: one reference to a function-local name gets a transposed spelling
    for name in unique_funcs:
        fn = functions[name][0]
        if in_test_module(lang, src, fn):
            continue
        local = declared_names(lang, src, fn)
        local.discard(receiver_name(lang, src, fn))
        for n in walk(fn):
            if n.type != "identifier":
                continue
            t = text(src, n)
            if t not in local or len(t) < 4 or is_declaration_site(lang, n):
                continue
            fname, _ = enclosing_function(lang, src, n)
            if fname != name:
                continue
            new = transpose_name(t)
            if not new or new in all_identifiers:
                continue
            line = src.count(b"\n", 0, n.start_byte) + 1
            out.append({
                "type": "renamed_identifier", "start": n.start_byte, "end": n.end_byte,
                "original": t, "replacement": new, "function": name, "line": line,
            })
    return {"parse_errors": has_errors(root), "functions": sorted(unique_funcs), "candidates": out}


def main():
    req = json.loads(sys.stdin.read())
    src = req["source"].encode("utf8")
    lang = req["lang"]
    if req["op"] == "candidates":
        res = candidates(lang, src)
    elif req["op"] == "check":
        tree = parser_for(lang).parse(src)
        res = {"parse_errors": has_errors(tree.root_node)}
    else:
        raise SystemExit(f"unknown op {req['op']}")
    res["ok"] = True
    sys.stdout.write(json.dumps(res))


if __name__ == "__main__":
    main()
