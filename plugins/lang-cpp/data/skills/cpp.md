C and C++ here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the statement, headers, sources and tests.
   - Per function: parameters, return type, how it fails (exception type, return code, `std::optional`, `NULL` in C).
   - The statement decides, never the name. "whether" returns `bool`; "find" may return an empty `std::optional` or `end()`.
   - When there may be no answer and the statement does not name a value for it, the declared return type decides: `std::optional<T>` gives `std::nullopt`, an iterator gives `end()`, a pointer gives `nullptr`. Never a made-up sentinel such as `-1`.
   - Keep the header's names and signatures.
   - Before coding, write the cases (section 2): one per sentence that states a behaviour, value, error or limit. A sentence with no case is a missed requirement.
   - Unfamiliar domain, format or protocol: when the research plugin is available, learn its rules and copy them into the cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page with the rules.
2. **Write whole functions, in slices.**
   - Simplest complete path first (one case, input to result). Run it.
   - Then each next behaviour from the statement, running after each.
   - New file, or a stub you have not changed yet: whole once, with its `#include`s, via `edit` `content`. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a function from its signature line to its closing `}`. Never leave part of the old body.
   - A signature change: edit header and source in the same step.
3. **Run the check file after every change** (section 2).
4. **Build and run the project's tests** (section 2). Fix the first error, run again.
5. **Before you finish**, check each requirement against the code, named errors included (exception type, message, return code).

A check disagrees with the statement: the statement wins. Fix the code, say which check is wrong.

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
- Offline, `FetchContent` fails. If `build/_deps/<name>-src` exists, add `-DFETCHCONTENT_FULLY_DISCONNECTED=ON`; else use the standard library.

**Checking your work.** One scratch file, `check.cpp`. Edit it, never re-type it.
- Inputs built in code.
- Expected values copied from the statement, never worked out by hand.
- One printed line per case, never `assert`.

```cpp
#include "solve.h"
#include <iostream>
#include <vector>

int main() {
    struct Case { std::vector<int> given, want; };
    std::vector<Case> cases{{{3, 1, 2}, {1, 2, 3}}};
    for (const auto& c : cases) {
        auto got = solve(c.given);
        std::cout << (got == c.want ? "ok " : "BAD") << " ->";
        for (int x : got) std::cout << ' ' << x;
        std::cout << "\n";
    }
}
```

```bash
c++ -std=c++17 -Wall -Wextra -g -fsanitize=address,undefined -Iinclude check.cpp src/solve.cpp -o /tmp/check && /tmp/check
```

- After any fix, run every case again, passing ones too.
- Keep the file while the work can still change; delete it when the user is done.
- A case fails: check the case first, against the statement. A case that breaks a stated rule, or holds a value you computed, is bad. Fix it, then the code.

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
