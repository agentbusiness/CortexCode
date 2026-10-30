C and C++ here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request and the header and source it changes. Callers only when a signature changes; then write.
   - Per function: parameters, return type, how it fails (exception type, return code, `std::optional`, `NULL` in C).
   - The request and the project decide, never a name alone. "whether" returns `bool`; "find" may return an empty `std::optional` or `end()`.
   - When there may be no answer and nothing names a value for it, the declared return type decides: `std::optional<T>` gives `std::nullopt`, an iterator gives `end()`, a pointer gives `nullptr`. Never a made-up sentinel such as `-1`.
   - Keep the header's names and signatures.
   - The request and the project disagree: say so and ask, instead of silently picking one.
   - Before coding, note the request's examples: they are the check, their values copied, never computed.
   - Unfamiliar domain, format, protocol or library: look it up (the research plugin, the docs, how others solved it):
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
2. **Write whole functions, in slices.**
   - Simplest complete path first (one case, input to result). Run it.
   - Then each next behaviour, running after each.
   - A new file is written whole, with its `#include`s, via `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a function from its signature line to its closing `}`. Never leave part of the old body.
   - A signature change: edit header and source in the same step.
3. **Run the tests after every change** (section 2).
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Build and run the project's tests (section 2); fix the first error, run again.
5. **Before you finish**, check each requirement against the code, named errors included (exception type, message, return code).

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- One owner: a member value or `std::unique_ptr<T>`. Shared ownership only when the lifetime truly is shared: `std::shared_ptr<T>`. Observers: a reference or raw pointer that never owns.
- A pointer, reference or iterator into a `std::vector` dies when it grows: keep an index, or use `std::deque` or `std::list` when elements must not move.
- Rule of zero: members own the resources, so the class needs no destructor, copy or move of its own; a class that owns a raw resource writes all five or deletes copying.
- Containers: `std::unordered_map`/`std::map` for lookup, `std::deque` for both ends, `std::priority_queue` for largest first, `std::set` for uniqueness.
- Standard building blocks first: `std::` containers, `std::make_unique`, `<algorithm>`, `std::chrono`, `std::string_view`; never hand-written `new`/`delete` bookkeeping.

## 2. Run, test, check

On macOS `cc`, `c++`, `gcc` and `g++` are all Apple clang with libc++: no `<bits/stdc++.h>`.

```bash
cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON         # configure once
cmake --build build                                            # build; --target name for one
ctest --test-dir build --output-on-failure                     # all tests
ctest --test-dir build --output-on-failure --no-tests=error -R 'Parser'   # matching tests
make && make test                                              # Makefile project; or make check
c++ -std=c++17 -fsyntax-only -Iinclude src/a.cpp               # quick type check of one file
cc -std=c11 -Wall -Wextra -g a.c -o /tmp/t && /tmp/t           # C, no build system
```

- `ctest` never builds: build first.
- Without `--no-tests=error`, a pattern matching nothing passes.
- One GoogleTest case: run its binary with `--gtest_filter='Suite.Name'`.
- Add a new `.cpp`/`.c` to the target's sources (`add_library`, `target_sources`, Make `SRCS`), or linking fails.
- clangd needs `compile_commands.json` from configure, else `no-compile-commands` and false errors. No CMake: `compile_flags.txt` at the root, one flag per line (`-std=c++17`, `-Iinclude`).
- clangd and the build disagree: the build is right.
- `FetchContent` downloads at configure time. A new dependency is a change to the user's project: ask before adding one.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: `tests/test_<name>.cpp` with a `main` that includes the project's header and checks each example, built and run from the project; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `std::vector<int>(18, 0)`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `if (got != want) std::cout << got << "\n";`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/`; git ignores `tmp/`.

## 3. Code that runs

Never return a reference, pointer or view into something that dies:

```cpp
std::string_view name() { std::string s = build(); return s; }   // bad: dangles
std::string name() { return build(); }                           // good
int& first = v[0]; v.push_back(x);              // bad: push_back may reallocate
v.push_back(x); int& first = v[0];
for (auto it = v.begin(); it != v.end(); ++it) if (bad(*it)) v.erase(it);   // bad
v.erase(std::remove_if(v.begin(), v.end(), bad), v.end());                   // good
```

Signed, unsigned and overflow:

```cpp
for (int i = 0; i < v.size() - 1; ++i)          // bad: size() - 1 wraps on an empty vector
for (std::size_t i = 0; i + 1 < v.size(); ++i)  // good
int p = a * b;                                  // bad: signed overflow is undefined behaviour
long long p = 1LL * a * b;                      // good; also 1LL << 40, accumulate(b, e, 0LL)
```

More traps:

```cpp
int count;                     // bad: uninitialized local or member
int count = 0;
if (m[key] > 0)                // bad: inserts a default entry
if (auto it = m.find(key); it != m.end() && it->second > 0)
std::sort(b, e, [](auto& x, auto& y) { return x.n <= y.n; });   // bad: not a strict order, can crash
std::sort(b, e, [](const auto& x, const auto& y) { return x.n < y.n; });
```

- Include what you use (`<algorithm>`, `<numeric>`, `<cstdint>`, `<stdexcept>`); indirect includes differ per compiler.
- C: `malloc(strlen(s) + 1)` for the `'\0'`; check `NULL`.
- C: `strcmp(a, b) == 0`, not `==`.
- C: `snprintf(buf, sizeof buf, ...)`, not `sprintf`/`strcpy`.

## 4. Errors

| Error (clang / gcc) [clangd code] | Fix |
|---|---|
| `unknown type name 'X'` / `'X' does not name a type` [`unknown_typename`] | include its header or qualify (`std::`) |
| `use of undeclared identifier` / `not declared in this scope` [`undeclared_var_use`] | include or declare it, or qualify |
| `no template named 'optional' in namespace 'std'` [`no_member_template`] | include `<optional>`, or raise `-std=c++17` / `CMAKE_CXX_STANDARD` |
| `'foo.h' file not found` [`pp_file_not_found`] | real only if the build fails (else clangd, section 2): add the include directory |
| `... is not marked const` / `discards qualifiers` [`member_function_call_bad_cvr`] | mark the method `const` in declaration and definition |
| `out-of-line definition ... does not match` / `no declaration matches` [`member_decl_does_not_match`] | copy the header's signature exactly |
| `no matching function for call` [`ovl_no_viable_function_in_call`], `too few arguments` [`typecheck_call_too_few_args`] | read the `note: candidate` lines; match the signature |
| `invalid operands to binary expression` / `no match for 'operator<'` [`typecheck_invalid_operands`] | define `bool operator<(const K&) const` (unordered containers: `==` and `std::hash`) |
| C: `call to undeclared function` / `implicit declaration of function` [`-Wimplicit-function-declaration`] | include the header or add a prototype |
| `comparison of integers of different signs` [`-Wsign-compare`] | `std::size_t` index or a range-for |
| `uninitialized when used` / `may be used uninitialized` [`-Wuninitialized`, `-Wmaybe-uninitialized`] | initialize at the declaration |
| `non-void function does not return a value` / `control reaches end of non-void function` [`-Wreturn-type`] | return on every path |
| `Undefined symbols` / `undefined reference to 'f(int)'` | no definition with that exact signature: write it, match the declaration (`const`, namespace), or add the `.cpp` to the build |
| `duplicate symbol` / `multiple definition of` | defined in a header: mark it `inline`, or move it to one `.cpp` |
| `Segmentation fault`, ASan `heap-buffer-overflow`, `heap-use-after-free` | rebuild with `-fsanitize=address,undefined -g`; its first project frame is the line |
