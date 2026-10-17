Go here: get it right, build and test it, code that runs, common errors and their fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the statement and the code (signatures, doc comments, stub bodies, `_test.go` files).
   - Per function: what it takes and returns, and which `error` when (a sentinel such as `ErrInvalid`; `nil` on success).
   - The statement decides, never the name. "whether" returns `bool`. "no results" may mean an empty slice, not `nil`.
   - When there may be no answer and the statement does not name a value for it, the signature decides: `(T, error)` returns the zero value and a non-nil `error`; `(T, bool)` returns the zero value and `false`. Never a made-up sentinel such as `-1`.
   - Keep existing names, signatures and the package name.
2. **Before coding, turn the statement into cases** for the check script (section 2).
   - Each sentence that states a behaviour, value, error or limit is one case. A shown requirement list is that list.
   - A sentence with no case is a requirement you will miss.
   - Unsure of a domain, format or protocol? Learn its rules with the research plugin; copy them into cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that states them.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the check script.
   - Add the next stated behaviour. Run again after every change.
   - New file, or a stub you have not changed yet: send it whole once with `edit` `content`, `package` line and imports included. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a function: replace it from its `func` line to its closing `}`. Never leave part of the old body.
   - `DuplicateDecl` after an edit: the old version is still there; delete it.
   - A `package main` program: `go run . arg1`.
4. **Run the project's tests** (section 2). Fix the first failure's cause, run again. Change only the code that failure runs through: a function whose cases pass stays as it is unless the task asks to change it.
5. **Before you finish**, check each requirement against the code, including named errors. Run `go vet ./...` and `gofmt -w` on changed files.

A check disagrees with the statement: the statement wins. Fix the code, name the wrong check.

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
- Offline, a missing module makes `go` hang. Prefix `GOPROXY=off` to fail at once.
- Use only the standard library and modules in `go.mod`; never `go install`. `GOPROXY=off go mod tidy` fixes `go.sum` when all is cached.
- go.mod's `go` line limits the language: `min`/`max`/`clear`, `slices` and `maps` need 1.21; `for i := range n` and a fresh loop variable per iteration need 1.22.
- A `go` line newer than the installed Go triggers a download: use `GOTOOLCHAIN=local` and report it. Never lower the line.
- Files with `//go:build tag` compile only with `-tags tag`.

**Checking your work.** One scratch file, `check_test.go`, in the package. Edit it; never re-type it.
- Build inputs in code.
- Copy expected values from the statement, never work them out yourself.
- Log one line per case with `t.Logf`, never `t.Fatal`: one run shows every case.

```go
package mypkg

import (
	"reflect"
	"testing"
)

func TestCheck(t *testing.T) {
	cases := []struct{ given, want []int }{{[]int{3, 1, 2}, []int{1, 2, 3}}}
	for _, c := range cases {
		got := Solve(c.given)
		mark := "ok "
		if !reflect.DeepEqual(got, c.want) {
			mark = "BAD"
		}
		t.Logf("%s %v -> %v want %v", mark, c.given, got, c.want)
	}
}
```

Run `go test -run '^TestCheck$' -v .`
- After any fix, run every case again, passing ones too. One fix can break another case.
- A case fails: check the case before the code. Re-derive it from the statement. A case that breaks a stated rule or holds a value you computed is bad: fix it first.
- Keep the file while the work can change. Delete it when the user is done.

## 3. Code that runs

Check every error; return the one the statement names:

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

- Empty list expected: return `[]T{}`, not `nil` (JSON `null`, and unequal in `reflect.DeepEqual`).
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
| `Foo redeclared in this block` (`DuplicateDecl`) | often a stub plus yours: keep one |
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
