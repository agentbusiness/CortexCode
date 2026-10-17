Rust here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the statement and the code (signatures, `///` docs, `todo!()` stubs, `mod tests`, `tests/`).
   - Per function: argument types, return type, how it fails (`Err` variant, `None`, panic).
   - The statement decides, never the name. "whether" returns `bool`. "find" may return `Option`. A named error is an `Err(...)`, not a panic.
   - When there may be no answer and the statement does not name a value for it, the signature decides: `Option<T>` gives `None`, `Result<T, E>` gives `Err(...)`. Never a sentinel such as `-1`, `0` or an empty value in their place.
   - Keep existing names, signatures and error enums.
2. **Before coding, turn the statement into cases** for the check script (section 2).
   - Each sentence that states a behaviour, value, error or limit is one case. A shown requirement list is that list.
   - A sentence with no case is a requirement you will miss.
   - Unsure of a domain, format or protocol? Learn its rules with the research plugin; copy them into cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that states them.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the check script.
   - Add the next stated behaviour. Run again after every change.
   - New file, or a stub you have not changed yet: send it whole once with `edit` `content`. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a function: replace it from its attributes and `fn` line to its closing `}`. Never leave part of the old body or a `todo!()`.
   - `unclosed delimiter` after an edit: a block was cut. Fix that first.
4. **Run the project's tests** (section 2). Fix the first failure's cause, run again. Change only the code that failure runs through: a function whose cases pass stays as it is unless the task asks to change it.
5. **Before you finish**, check each requirement against the code, including named errors (variant and message). `grep -rn "todo!\|dbg!" src tests` must be empty.

A check disagrees with the statement: the statement wins. Fix the code, name the wrong check.

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
- Offline: add `--offline`. `failed to download`, `no matching package`: not cached; use the standard library.
- Only crates already in `Cargo.toml`. Never `cargo update`, never edit `Cargo.lock`.
- `[workspace]`: pick a member with `-p name`.
- `tests/` sees only `pub` items of `src/lib.rs`, as `my_crate::item` (dashes become `_`).
- A new file compiles only after `mod name;` in its parent.

**Checking your work.** One scratch file, `examples/check.rs` (library crate). Edit it; never re-type it.
- Build inputs in code.
- Copy expected values from the statement, never work them out yourself.
- Print one line per case, no `assert!`: one run shows every case.

```rust
use mycrate::solve;

fn main() {
    let cases: Vec<(Vec<i32>, Vec<i32>)> = vec![(vec![3, 1, 2], vec![1, 2, 3]), ((0..5).chain(0..5).collect(), (0..5).chain(0..5).collect())];
    for (given, want) in cases {
        let got = solve(&given);
        println!("{} {:?} -> {:?} want {:?}", if got == want { "ok " } else { "BAD" }, given, got, want);
    }
}
```

Run `cargo run --example check`. Binary crate: put the loop in a `#[test] fn check()` in the existing `mod tests` (a second one is E0428); run `cargo test check -- --nocapture`.
- After any fix, run every case again, passing ones too. One fix can break another case.
- A case fails: check the case before the code. Re-derive it from the statement. A case that breaks a stated rule or holds a value you computed is bad: fix it first.
- Keep the file while the work can change. Delete it when the user is done.

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
- Types compared in tests need `#[derive(Debug, Clone, PartialEq)]`.

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
