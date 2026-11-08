//! Bounded intraprocedural reaching definitions, computed only for a requested function.
//! Branches join sets of possible definitions. Unsupported effects remain explicit boundaries.

use cc_lang_kit::cc_protocol::{
    RpcError,
    contracts::lang::{ValueInput, ValueStep, ValueTrace, ValueTraceParams},
};
use cc_lang_kit::syntax::{field, named_children, parse};
use cc_lang_kit::tree_sitter::Node;
use std::collections::{BTreeMap, BTreeSet};

type State = BTreeMap<String, BTreeSet<u32>>;
const UNKNOWN: u32 = 0;
const MAX_NODES: usize = 2000;

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    src.get(n.byte_range()).unwrap_or("")
}
fn contains(n: Node, line: u32) -> bool {
    n.start_position().row < line as usize && n.end_position().row + 1 >= line as usize
}

fn function_at(n: Node<'_>, line: u32) -> Option<Node<'_>> {
    let mut pending = vec![(n, 0usize)];
    let mut found = None;
    let mut deepest = 0;
    while let Some((node, depth)) = pending.pop() {
        if !contains(node, line) {
            continue;
        }
        if node.kind() == "function_definition" && depth >= deepest {
            found = Some(node);
            deepest = depth;
        }
        pending.extend(
            named_children(node)
                .into_iter()
                .map(|child| (child, depth + 1)),
        );
    }
    found
}

fn names(n: Node, src: &str, out: &mut BTreeSet<String>) {
    let mut pending = vec![n];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" => {
                out.insert(text(node, src).to_owned());
            }
            "attribute" => {
                if let Some(base) = field(node, "object") {
                    pending.push(base);
                }
            }
            "keyword_argument" => {
                if let Some(value) = field(node, "value") {
                    pending.push(value);
                }
            }
            "function_definition" | "class_definition" | "lambda" => {}
            _ => pending.extend(named_children(node)),
        }
    }
}

fn targets(n: Node, src: &str, out: &mut BTreeSet<String>) {
    let mut pending = vec![n];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" => {
                out.insert(text(node, src).to_owned());
            }
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "tuple"
            | "list"
            | "list_splat_pattern"
            | "dictionary_splat_pattern" => pending.extend(named_children(node)),
            _ => {}
        }
    }
}

fn join(a: &State, b: &State) -> State {
    let keys: BTreeSet<_> = a.keys().chain(b.keys()).cloned().collect();
    keys.into_iter()
        .map(|key| {
            let mut defs = a
                .get(&key)
                .cloned()
                .unwrap_or_else(|| BTreeSet::from([UNKNOWN]));
            defs.extend(
                b.get(&key)
                    .cloned()
                    .unwrap_or_else(|| BTreeSet::from([UNKNOWN])),
            );
            (key, defs)
        })
        .collect()
}

struct Analysis<'a> {
    src: &'a str,
    query_line: u32,
    query_name: &'a str,
    steps: Vec<ValueStep>,
    observed: BTreeSet<u32>,
    observed_here: bool,
    notes: BTreeSet<String>,
    truncated: bool,
    depth: usize,
}

impl Analysis<'_> {
    fn observe(&mut self, node: Node, state: &State) {
        if !self.observed_here && contains(node, self.query_line) {
            self.observed_here = true;
            self.observed.extend(
                state
                    .get(self.query_name)
                    .cloned()
                    .unwrap_or_else(|| BTreeSet::from([UNKNOWN])),
            );
        }
    }

    fn event(
        &mut self,
        node: Node,
        defs: BTreeSet<String>,
        uses: BTreeSet<String>,
        state: &mut State,
        controls: &[u32],
    ) -> u32 {
        if self.steps.len() >= MAX_NODES {
            self.truncated = true;
            for name in defs {
                state.insert(name, BTreeSet::from([UNKNOWN]));
            }
            return UNKNOWN;
        }
        let id = self.steps.len() as u32 + 1;
        let mut cut = uses.len() > 16 || controls.len() > 16 || defs.len() > 32;
        let mut inputs = Vec::new();
        for name in uses.into_iter().take(16) {
            let set = state.get(&name);
            let shortened = name.chars().count() > 128;
            let omitted = set.is_some_and(|s| s.len() > 32);
            cut |= shortened || omitted;
            inputs.push(ValueInput {
                name: name.chars().take(128).collect(),
                definitions: set
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|d| *d != UNKNOWN)
                    .take(32)
                    .collect(),
                unresolved: shortened || omitted || set.is_none_or(|s| s.contains(&UNKNOWN)),
            });
        }
        if !controls.is_empty() {
            inputs.push(ValueInput {
                name: "branch condition".into(),
                definitions: controls
                    .iter()
                    .copied()
                    .filter(|id| *id != UNKNOWN)
                    .take(16)
                    .collect(),
                unresolved: controls.contains(&UNKNOWN) || controls.len() > 16,
            });
        }
        if cut {
            self.truncated = true;
        }
        let snippet = text(node, self.src);
        self.steps.push(ValueStep {
            id,
            line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            text: snippet.chars().take(500).collect(),
            defines: defs
                .iter()
                .take(32)
                .map(|s| s.chars().take(128).collect())
                .collect(),
            inputs,
        });
        if snippet.chars().count() > 500 {
            self.notes.insert(
                "Some source statements are shortened; read their full source before editing."
                    .into(),
            );
        }
        for name in defs {
            state.insert(name, BTreeSet::from([id]));
        }
        id
    }

    fn block(&mut self, node: Node, state: &mut State, controls: &[u32]) {
        for child in named_children(node) {
            self.statement(child, state, controls);
        }
    }

    fn statement(&mut self, node: Node, state: &mut State, controls: &[u32]) {
        if self.depth >= 128 {
            self.truncated = true;
            for origins in state.values_mut() {
                origins.insert(UNKNOWN);
            }
            if contains(node, self.query_line) {
                self.observed_here = true;
                self.observed.insert(UNKNOWN);
            }
            self.notes.insert(
                "Nested control flow exceeded the analysis limit; read this function's source."
                    .into(),
            );
            return;
        }
        self.depth += 1;
        self.statement_inner(node, state, controls);
        self.depth -= 1;
    }

    fn statement_inner(&mut self, node: Node, state: &mut State, controls: &[u32]) {
        if self.steps.len() >= MAX_NODES {
            self.truncated = true;
            return;
        }
        // Compound blocks observe only the actual nested statement, not the state before the block.
        match node.kind() {
            "if_statement" | "elif_clause" => {
                let cond = field(node, "condition");
                let mut next_controls = controls.to_vec();
                if let Some(cond) = cond {
                    self.observe(cond, state);
                    let mut used = BTreeSet::new();
                    names(cond, self.src, &mut used);
                    next_controls.push(self.event(cond, BTreeSet::new(), used, state, controls));
                }
                let before = state.clone();
                let mut yes = before.clone();
                if let Some(body) = field(node, "consequence") {
                    self.block(body, &mut yes, &next_controls);
                }
                let mut combined = yes;
                let mut has_else = false;
                // Python keeps every elif/else as a sibling alternative, not a nested chain.
                for other in named_children(node)
                    .into_iter()
                    .filter(|n| matches!(n.kind(), "elif_clause" | "else_clause"))
                {
                    let mut branch = before.clone();
                    if other.kind() == "elif_clause" {
                        if let Some(cond) = field(other, "condition") {
                            self.observe(cond, &branch);
                            let mut used = BTreeSet::new();
                            names(cond, self.src, &mut used);
                            let condition = self.event(
                                cond,
                                BTreeSet::new(),
                                used,
                                &mut branch,
                                &next_controls,
                            );
                            next_controls.push(condition);
                        }
                        if let Some(body) = field(other, "consequence") {
                            self.block(body, &mut branch, &next_controls);
                        }
                    } else {
                        has_else = true;
                        for child in named_children(other) {
                            if child.kind() == "block" {
                                self.block(child, &mut branch, &next_controls);
                            }
                        }
                    }
                    combined = join(&combined, &branch);
                }
                if !has_else {
                    combined = join(&combined, &before);
                }
                *state = combined;
            }

            "for_statement" | "while_statement" => {
                self.notes.insert("Only zero or one pass through a loop is followed. Later passes may change the value; read the loop source.".into());
                let before = state.clone();
                self.observe_header(node, state);
                let mut body_state = state.clone();
                if let Some(left) = field(node, "left") {
                    let mut defs = BTreeSet::new();
                    targets(left, self.src, &mut defs);
                    let mut used = BTreeSet::new();
                    if let Some(right) = field(node, "right") {
                        names(right, self.src, &mut used);
                    }
                    self.event(left, defs, used, &mut body_state, controls);
                }
                for child in named_children(node) {
                    if child.kind() == "block" {
                        self.block(child, &mut body_state, controls);
                    }
                }
                if let Some(other) = field(node, "alternative") {
                    self.uncertain_blocks(other, &mut body_state, controls);
                }
                // Mark loop-written values as additionally unknown, rather than claim one iteration is complete.
                *state = join(&before, &body_state);
                for (name, defs) in state.iter_mut() {
                    if before.get(name) != body_state.get(name) {
                        defs.insert(UNKNOWN);
                    }
                }
                if contains(node, self.query_line) {
                    self.observed.insert(UNKNOWN);
                }
            }
            "try_statement" | "with_statement" | "match_statement" => {
                self.notes.insert("Exception handling, context managers and pattern matching are not resolved; affected values have unknown alternatives.".into());
                self.observe_header(node, state);
                let before = state.clone();
                self.uncertain_blocks(node, state, controls);
                *state = join(&before, state);
                for defs in state.values_mut() {
                    defs.insert(UNKNOWN);
                }
                if contains(node, self.query_line) {
                    self.observed.insert(UNKNOWN);
                }
            }
            "function_definition" | "class_definition" | "decorated_definition" => {
                self.observe(node, state);
                if let Some(name) = field(node, "name") {
                    let mut defs = BTreeSet::new();
                    targets(name, self.src, &mut defs);
                    self.event(node, defs, BTreeSet::new(), state, controls);
                }
            }
            _ => {
                self.observe(node, state);
                let expr = if node.kind() == "expression_statement" {
                    named_children(node).first().copied().unwrap_or(node)
                } else {
                    node
                };
                let mut defs = BTreeSet::new();
                let mut uses = BTreeSet::new();
                match expr.kind() {
                    "assignment" | "augmented_assignment" => {
                        if let Some(left) = field(expr, "left") {
                            targets(left, self.src, &mut defs);
                            if defs.is_empty() {
                                names(left, self.src, &mut uses);
                                self.notes.insert("Changing an object field or list item may affect other names for the same object; those changes are not followed.".into());
                                for name in &uses {
                                    state.entry(name.clone()).or_default().insert(UNKNOWN);
                                }
                            }
                            if expr.kind() == "augmented_assignment" {
                                names(left, self.src, &mut uses);
                            }
                        }
                        if let Some(right) = field(expr, "right") {
                            names(right, self.src, &mut uses);
                            if has_kind(right, &["assignment"]) {
                                self.notes.insert("Chained assignments are only partly resolved; inspect all targets in the source.".into());
                            }
                        } else {
                            // An annotation without a value does not assign a local variable.
                            defs.clear();
                        }
                        self.event(node, defs, uses, state, controls);
                    }
                    "global_statement" | "nonlocal_statement" | "delete_statement" => {
                        names(expr, self.src, &mut defs);
                        for name in defs {
                            state.insert(name, BTreeSet::from([UNKNOWN]));
                        }
                        self.notes
                            .insert("External-scope writes and deletion are not resolved.".into());
                    }
                    _ => {
                        names(expr, self.src, &mut uses);
                        self.event(node, defs, uses, state, controls);
                    }
                }
                if has_kind(expr, &["call", "await", "yield"]) {
                    self.notes.insert("Calls, awaited work and yields can have effects outside this function; those effects are not traced.".into());
                    let mut used = BTreeSet::new();
                    names(expr, self.src, &mut used);
                    for name in used {
                        if let Some(origins) = state.get_mut(&name) {
                            origins.insert(UNKNOWN);
                        }
                    }
                }
                if has_kind(
                    expr,
                    &[
                        "lambda",
                        "list_comprehension",
                        "dictionary_comprehension",
                        "set_comprehension",
                        "generator_expression",
                        "named_expression",
                    ],
                ) {
                    self.notes.insert("Nested expressions with their own scope or assignments are not fully resolved.".into());
                }
            }
        }
    }

    fn observe_header(&mut self, node: Node, state: &State) {
        if node.start_position().row as u32 + 1 == self.query_line {
            self.observe(node, state);
        }
    }

    fn uncertain_blocks(&mut self, node: Node, state: &mut State, controls: &[u32]) {
        for child in named_children(node) {
            if child.kind() == "block" {
                self.block(child, state, controls);
            } else if matches!(
                child.kind(),
                "except_clause" | "else_clause" | "finally_clause" | "case_clause"
            ) {
                self.uncertain_blocks(child, state, controls);
            }
        }
    }
}

fn has_kind(node: Node, kinds: &[&str]) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if kinds.contains(&node.kind()) {
            return true;
        }
        pending.extend(named_children(node));
    }
    false
}

pub fn trace(p: ValueTraceParams) -> Result<ValueTrace, RpcError> {
    if p.file.text.len() > 512 * 1024
        || p.line == 0
        || p.name.is_empty()
        || p.name.len() > 128
        || !p.name.chars().enumerate().all(|(i, c)| {
            c == '_'
                || (if i == 0 {
                    c.is_alphabetic()
                } else {
                    c.is_alphanumeric()
                })
        })
    {
        return Err(RpcError::invalid_params(
            "Use one local variable, a positive line and a Python file under 512 KiB.",
        ));
    }
    let tree = parse(&tree_sitter_python::LANGUAGE.into(), &p.file.text).ok_or_else(|| {
        RpcError::invalid_params("Could not parse this Python file; read its source.")
    })?;
    let function = function_at(tree.root_node(), p.line)
        .ok_or_else(|| RpcError::invalid_params("Choose a line inside one Python function."))?;
    let mut a = Analysis { src: &p.file.text, query_line: p.line, query_name: &p.name, steps: Vec::new(), observed: BTreeSet::new(), observed_here: false,
        notes: BTreeSet::from(["Possible assignments before this line, inside one function. This does not run the code. Inputs from callers or other functions may be unknown. Paths that return early may still be included.".into()]), truncated: false, depth: 0 };
    let mut state = State::new();
    if function.has_error() {
        a.notes
            .insert("This function has parse errors; the trace is incomplete.".into());
    }
    if let Some(parameters) = field(function, "parameters") {
        for param in named_children(parameters) {
            let name = if param.kind() == "identifier" {
                Some(param)
            } else {
                field(param, "name").or_else(|| {
                    named_children(param)
                        .into_iter()
                        .find(|n| n.kind() != "type")
                })
            };
            if let Some(name) = name {
                let mut defs = BTreeSet::new();
                targets(name, &p.file.text, &mut defs);
                a.event(
                    name,
                    defs,
                    BTreeSet::from(["caller-supplied input".into()]),
                    &mut state,
                    &[],
                );
            }
        }
    }
    if let Some(body) = field(function, "body") {
        a.block(body, &mut state, &[]);
    }
    if !a.observed_here {
        if a.truncated {
            a.observed.insert(UNKNOWN);
        } else {
            return Err(RpcError::invalid_params(
                "Choose the line of a statement that reads the variable, not a blank line or function header.",
            ));
        }
    }
    if a.observed.contains(&UNKNOWN) {
        a.notes.insert(
            "At least one possible origin is unknown or this variable may be unassigned on a path."
                .into(),
        );
    }
    let roots: Vec<_> = a
        .observed
        .iter()
        .copied()
        .filter(|id| *id != UNKNOWN)
        .collect();
    let limit = p.max_steps.clamp(1, 80) as usize;
    let mut needed = BTreeSet::new();
    let mut pending = roots.clone();
    while let Some(id) = pending.pop() {
        if needed.contains(&id) {
            continue;
        }
        if needed.len() == limit {
            a.truncated = true;
            break;
        }
        needed.insert(id);
        if let Some(step) = a.steps.get(id.saturating_sub(1) as usize) {
            for input in &step.inputs {
                pending.extend(input.definitions.iter().copied());
            }
        }
    }
    let steps = a
        .steps
        .into_iter()
        .filter(|s| needed.contains(&s.id))
        .collect();
    Ok(ValueTrace {
        path: p.file.path,
        function: field(function, "name")
            .map(|n| text(n, &p.file.text))
            .unwrap_or("")
            .into(),
        line: p.line,
        name: p.name,
        roots,
        steps,
        boundaries: a.notes.into_iter().collect(),
        truncated: a.truncated,
        content_hash: String::new(),
    })
}
