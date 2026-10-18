Go here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request, then what the project already says about it: the code it touches (signatures, doc comments), its `_test.go` files, docs and callers.
   - Per function: what it takes and returns, and which `error` when (a sentinel such as `ErrInvalid`; `nil` on success).
   - The request and the project decide, never a name alone. "whether" returns `bool`.
   - When there may be no answer and nothing names a value for it, the signature decides: `(T, error)` returns the zero value and a non-nil `error`; `(T, bool)` returns the zero value and `false`. Never a made-up sentinel such as `-1`.
   - Keep existing names, signatures and the package name.
   - The request and the project disagree: say so and ask, instead of silently picking one.
2. **Before coding, note the request's examples: they are the check, their values copied, never computed.**
   - Each behaviour, value, error or limit the request names is one case. A shown requirement list is that list.
   - Unsure of a domain, format, protocol or module? Look it up: the research plugin, pkg.go.dev, how others solved it:
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the tests.
   - Add the next behaviour. Run again after every change.
   - A new file is written whole with `edit` `content`, `package` line and imports included; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a function: replace it from its `func` line to its closing `}`. Never leave part of the old body.
   - `DuplicateDecl` after an edit: the old version is still there; delete it.
   - A `package main` program: `go run . arg1`.
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests (section 2); fix the first failure's cause, run again.
5. **Before you finish**, check each requirement against the code, including named errors. Run `go vet ./...` and `gofmt -w` on the files you changed.

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- A struct stored or passed by value is a copy: methods that change it take a pointer receiver, and every method of the type uses the same receiver kind.
- Slices and maps share their backing data: a returned slice lets the caller change yours, and `append` may or may not reallocate. Copy (`slices.Clone`, `maps.Clone`) when it must not be shared.
- Shared between goroutines: one owning goroutine with channels, or a `sync.Mutex` declared beside the data it guards.
- Containers: `map[K]V` for lookup, a slice for order, `container/list` for removal in the middle, `container/heap` for smallest first, `map[K]struct{}` for uniqueness.
- Standard building blocks first: `slices`, `maps`, `strings`, `sort`, `container/heap`, `time`, `sync`, `encoding/json`, before writing your own.

## 2. Run, test, check

Run `go` in the directory of `go.mod`.

```bash
go build ./...                                   # compile everything except tests
go vet ./...                                     # also compiles tests; finds common mistakes
go test ./...                                    # all packages
go test ./internal/parser                        # one package: a directory, never a file
go test -run '^TestParse$' -v ./internal/parser  # one test
go test -run '^TestParse$/^empty_input$' ./internal/parser   # one subtest
go test -count=1 -race ./...                     # no cached results, race detector
```

- `-run` is a regex split on `/` per subtest level. Spaces in subtest names become `_`.
- A filter matching nothing exits 0. List names: `go test -list . ./pkg`.
- Prefer the standard library and the modules in `go.mod`. A new module is a new dependency of the user's project, and `go install` puts a program on the machine: ask before either. `go mod tidy` fixes `go.sum`.
- go.mod's `go` line limits the language: `min`/`max`/`clear`, `slices` and `maps` need 1.21; `for i := range n` and a fresh loop variable per iteration need 1.22.
- A `go` line newer than the installed Go triggers a download: use `GOTOOLCHAIN=local` and report it. Never lower the line.
- Files with `//go:build tag` compile only with `-tags tag`.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: a `<name>_test.go` next to the code, run with `go test`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `make([]int, 18)`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `t.Errorf("got %v, want %v", got, want)`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/`; git ignores `tmp/`.

## 3. Code that runs

Check every error; return the one the request or the project's API names:

```go
n, _ := strconv.Atoi(s)                    // bad: bad input silently becomes 0
n, err := strconv.Atoi(s)
if err != nil {
	return 0, fmt.Errorf("parse %q: %w", s, err)   // good: %w keeps errors.Is working
}
```

Compare errors with `errors.Is(err, ErrX)`, never `err.Error()` strings.

Writing to a nil map panics:

```go
var counts map[string]int; counts[k]++     // bad
counts := map[string]int{}                 // good; create inner maps too
```

Slices share their backing array:

```go
out = append(out, cur); cur = cur[:0]      // bad: every chunk in out is overwritten
out = append(out, append([]int(nil), cur...)); cur = cur[:0]   // good: a copy
b := a[:2]; b = append(b, 9)               // bad: overwrites a[2]
b := a[:2:2]; b = append(b, 9)             // good: capacity 2, append copies
for _, v := range xs { v.N++ }             // bad: changes a copy
for i := range xs { xs[i].N++ }            // good
```

Loop variables and `:=`:

```go
for _, v := range items { go func() { use(v) }() }   // go line < 1.22: every goroutine sees the last v
for _, v := range items { go func(v Item) { use(v) }(v) }
x, err := f()        // inside if/for: a NEW x shadows the outer one
x, err = f()         // assigns the outer x
```

- An empty slice and `nil` differ in JSON (`[]` and `null`) and in `reflect.DeepEqual`: return what callers and the project's API expect.
- Map iteration order is random: collect the keys and sort them.
- `len(s)` and `s[i]` are bytes. For characters: `for _, r := range s` or `[]rune(s)`.
- Integer overflow wraps silently: `int64` for large sums and products.
- Goroutines: `wg.Add(1)` before `go`, `defer wg.Done()` inside. Write results to `out[i]`, not a shared `append`. Never fix a race with `time.Sleep`.

## 4. Errors

| Error (gopls code) | Usual cause and fix |
|---|---|
| `declared and not used: x` (`UnusedVar`) | use it; a `:=` in `if`/`for` made a new variable: use `=`. Never `_ = x` |
| `"os" imported and not used` (`UnusedImport`) | remove the import |
| `undefined: Foo` (`UndeclaredName`, `UndeclaredImportedName`) | wrong case, or a missing import or qualifier |
| `x.Foo undefined (type T has no field or method Foo)` (`MissingFieldOrMethod`) | real names: `read outline://<path>` |
| `Foo redeclared in this block` (`DuplicateDecl`) | two versions of the same declaration: keep one |
| `cannot use x (variable of type int) as string value` (`IncompatibleAssign`) | convert: `strconv.Itoa(x)`, not `string(x)` |
| `mismatched types int and int64` (`MismatchedTypes`) | `int64(a) + b` |
| `X does not implement I (method M has pointer receiver)` (`InvalidIfaceAssign`) | the exact signature; pass `&X{}` |
| `not enough arguments in call` (`WrongArgCount`), `assignment mismatch` (`WrongAssignCount`), `too many return values` (`WrongResultCount`) | match the `want` signature: `read definition://path:L:C` |
| `missing return` (`MissingReturn`) | every path ends in `return` or `panic` |
| `no new variables on left side of :=` (`NoNewVar`) | `=` |
| `min requires go1.21 or later` (`UnsupportedFeature`) | rewrite for go.mod's `go` line |
| `could not import P` (`BrokenImport`), `no required module provides package P` | own packages start with go.mod's `module` path; others must be in `go.mod` |
| panic `invalid memory address or nil pointer dereference` | a nil pointer, often used before checking `err` |
| panic `index out of range` | off-by-one or empty input: check `len` |
