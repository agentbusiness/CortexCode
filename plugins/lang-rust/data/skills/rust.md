Rust here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request, then what the project already says about it: the code it touches (signatures, `///` docs), `mod tests`, `tests/`, docs and callers.
   - Per function: argument types, return type, how it fails (`Err` variant, `None`, panic).
   - The request and the project decide, never a name alone. "whether" returns `bool`. "find" may return `Option`. A named error is an `Err(...)`, not a panic.
   - When there may be no answer and nothing names a value for it, the signature decides: `Option<T>` gives `None`, `Result<T, E>` gives `Err(...)`. Never a sentinel such as `-1`, `0` or an empty value in their place.
   - Keep existing names, signatures and error enums.
   - The request and the project disagree: say so and ask, instead of silently picking one.
2. **Before coding, note the request's examples: they are the check, their values copied, never computed.**
   - Each behaviour, value, error or limit the request names is one case. A shown requirement list is that list.
   - Unsure of a domain, format, protocol or crate? Look it up: the research plugin, docs.rs, how others solved it:
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the tests.
   - Add the next behaviour. Run again after every change.
   - A new file is written whole with `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a function: replace it from its attributes and `fn` line to its closing `}`. Never leave part of the old body or a `todo!()` of yours.
   - `unclosed delimiter` after an edit: a block was cut. Fix that first.
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests (section 2); fix the first failure's cause, run again.
5. **Before you finish**, check each requirement against the code, including named errors (variant and message). Your own diff holds no `todo!()` or `dbg!`.

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- One owner: a field or `Box<T>`. Shared and read-only: `&T`, `Rc<T>` (`Arc<T>` across threads). Shared and changed: `Rc<RefCell<T>>` (`Arc<Mutex<T>>` across threads).
- `Cell<T>` only for small `Copy` values: it never lends `&T`. `RefCell<T>` lends through `borrow()` and `borrow_mut()`, checked at run time.
- Nodes that point at each other and must hand out `&T` or `&mut T`: each node owned through `Box`, the links as `NonNull<Node<T>>` or raw pointers in `unsafe` blocks with the invariants in `// SAFETY:` comments, freed in `Drop`.
- `Rc`, `Cell` and `RefCell` make a type neither `Send` nor `Sync`: across threads use `Arc` with `Mutex` or `RwLock`.
- Containers: `HashMap`/`BTreeMap` for lookup, `VecDeque` for both ends, `BinaryHeap` for largest first, `HashSet` for uniqueness.
- Standard building blocks first: allocate behind a raw pointer with `Box::into_raw`, take back and free with `Box::from_raw`, never `std::alloc` by hand; `NonNull<T>` for a non-null raw pointer.

## 2. Run, test, check

```bash
cargo check --message-format=short               # fast type and borrow check
cargo run -- arg1 arg2                           # build and run the binary
cargo test                                       # unit tests, tests/, doc examples
cargo test parse                                 # tests whose path contains "parse"
cargo test --lib tests::parse_empty -- --exact   # exactly one test
cargo test name -- --nocapture                   # show println! output
cargo clippy --all-targets -- -D warnings        # lint
```

- Cargo flags (`-p`, `--lib`, `--test`, `--features`) go before `--`; test flags (`--exact`, `--nocapture`) after.
- A filter matching nothing passes (`0 passed; 5 filtered out`). Real names: `cargo test -- --list`.
- Prefer the crates already in `Cargo.toml` and the standard library. A new crate is a new dependency of the user's project: ask before adding it. `cargo update` only when the user asks.
- `[workspace]`: pick a member with `-p name`.
- `tests/` sees only `pub` items of `src/lib.rs`, as `my_crate::item` (dashes become `_`).
- A new file compiles only after `mod name;` in its parent.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: `tests/<name>.rs` that uses the crate, run with `cargo test`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `vec![0; 18]`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `assert_eq!(got, want)`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/`; git ignores `tmp/`.

## 3. Code that runs

Return named failures as `Err`, never panic:

```rust
fn parse(s: &str) -> u32 { s.parse().unwrap() }                  // bad: panics on bad input
fn parse(s: &str) -> Result<u32, ParseError> {                   // good
    s.trim().parse().map_err(|_| ParseError::InvalidNumber)
}
```

Borrow, do not move (E0382). Take `&str` and `&[T]`, return owned `String` and `Vec`:

```rust
for x in v { ... } v.len()          // bad: the loop moved v
for x in &v { ... }                 // good; &mut v to change items
```

Do not mutate a collection while iterating it (E0502). Collect, then apply:

```rust
for x in v.iter() { if *x > 0 { v.push(x * 2); } }                         // bad
let extra: Vec<i32> = v.iter().filter(|x| **x > 0).map(|x| x * 2).collect();
v.extend(extra);                                                            // good
```

Handle `None` and `Err` where they arise:

```rust
let first = v.first().unwrap();                      // bad: panics on empty input
let Some(first) = v.first() else { return None };    // good; or `?` in a fn returning Option
```

Integer overflow panics in debug builds, so in every test:

```rust
a - b                         // usize: panics when b > a
a.checked_sub(b)              // or a.abs_diff(b), or compute in i64
x * y                         // u32/i32 products overflow early: widen to u64/i128 first
big as u8                     // silently truncates: u8::try_from(big)
```

- `s.len()` counts bytes: `s.chars().count()`.
- `&s[0..3]` panics off a char boundary: `s.chars().take(3).collect::<String>()`.
- Parse with `line.trim().parse::<u32>()`.
- `HashMap`/`HashSet` order is random: sort keys, or use `BTreeMap`.
- `-7 % 3 == -1`; `(-7i32).rem_euclid(3)` is 2.
- Iterators are lazy: `v.iter().map(f);` does nothing.
- Types compared with `assert_eq!` need `Debug` and `PartialEq`.

## 4. Errors

| Error | Usual cause and fix |
|---|---|
| E0382 use of moved value | borrow (`&v`, `&str`); `.clone()` only if both need ownership |
| E0499, E0502, E0505, E0506 borrow conflict | a borrow is still live: move the conflicting use after it, or collect then apply |
| E0507 cannot move out of borrowed content | `&s.name`, `.clone()`, `.take()`, `std::mem::take`, `.as_ref()` |
| E0515 returns a reference to a local | return the owned `String`/`Vec` |
| E0597, E0716 does not live long enough | bind it in the outer scope: `let s = make(); let r = s.as_str();` |
| E0384, E0596 cannot assign / borrow as mutable | `let mut x`; `&mut self` |
| E0308 expected `T`, found `Option<T>`/`Result` | `?`, `match`, `.unwrap_or(d)`, `.ok_or(err)?` |
| E0308 expected `T`, found `()` | drop the `;` after the returned value, or add the missing `else` |
| E0277 trait bound not satisfied | `usize` vs `i32`: `usize::try_from(x)?`; `?` needs a fn returning `Result`/`Option`; add the derive |
| E0599 no method found | trait not in scope (`use std::io::Write;`, `use std::fmt::Write;`), or derive/bound missing |
| E0425, E0412, E0433, E0432 cannot find / unresolved | `use crate::module::Item;` (path: `read symbol://Name`); in `mod tests` `use super::*;` |
| E0004 non-exhaustive patterns | add the arms the error names |
| E0282 type annotations needed | `let v: Vec<u32> = ...`, `s.parse::<u64>()` |
| E0428 defined multiple times | a duplicate item, often a second `mod tests`: keep one |
| `unlinked-file` (rust-analyzer) | add `mod name;` to the parent module |
| panic `called Option::unwrap() on a None value` | handle `None` where it is produced |
| panic `index out of bounds` | off-by-one or empty input: check `len()`, or `.get(i)` |
