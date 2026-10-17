Java here: get it right, build and test it, code that runs, compiler and runtime errors and fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the statement and the code (signatures, stubs, tests).
   - Per method or class: what it takes, returns and throws (exception type and message).
   - The statement decides, never the name. "whether" returns a `boolean`. "find" may return `null` or an `Optional`, as stated. "returns the sorted list" means a new list, not a sorted argument.
   - When there may be no answer and the statement does not name a value for it, the declared return type decides: `Optional<T>` gives `Optional.empty()`, an object type gives `null`. Not `-1` or an empty collection unless stated.
   - Keep existing class names, packages and signatures.
2. **Before coding, turn the statement into cases** for the check script (section 2).
   - Each sentence that states a behaviour, value, error or limit is one case. A shown requirement list is that list.
   - A sentence with no case is a requirement you will miss.
   - Unsure of a domain, format or protocol? Learn its rules with the research plugin; copy them into cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that states them.
3. **Write whole methods and classes, in slices.**
   - First the simplest complete path (one case, input to result). Compile, run the check script.
   - Add the next stated behaviour. Run again after every change.
   - New class, or a stub you have not changed yet: send it whole once with `edit` `content`. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a method: replace it from its signature line to its closing `}`. Never leave part of the old body or a stray brace.
4. **Run the project's tests** (section 2). Fix the first failure's cause, run again. Change only the code that failure runs through: a function whose cases pass stays as it is unless the task asks to change it.
5. **Before you finish**, check each requirement against the code, including the named errors.

A check disagrees with the statement: the statement wins. Fix the code, name the wrong check.

## 2. Run, test, check

Use the project's wrapper when it exists (`./gradlew`, `./mvnw`; `Permission denied`: `chmod +x gradlew`). Always offline: nothing can be downloaded.

```bash
./gradlew classes --offline -q                                    # compile main classes
./gradlew classes testClasses --offline -q                        # compile with tests
./gradlew test --offline --tests 'com.acme.FooTest'               # one class
./gradlew test --offline --tests 'com.acme.FooTest.parsesEmpty'   # one method
mvn -q -o compile                                                 # Maven compile (./mvnw the same)
mvn -q -o test-compile                                            # Maven compile with tests
mvn -q -o test -Dtest='FooTest#parsesEmpty'                       # Maven one method
javac -d out $(find src/main/java -name '*.java')                 # no build tool
java Main.java                                                    # run one source file, no compile step
```

- `> Task :test UP-TO-DATE`: nothing ran; add `--rerun`.
- Gradle prints only the failing test and line. The full message is in `build/test-results/test/TEST-<class>.xml`.
- `Operation not permitted` under `~/.gradle` or `~/.m2`: the caches are outside the write sandbox. Rerun the same command with `write_outside: ["~/.gradle"]` (or `["~/.m2"]`).
- `No cached version ... available for offline mode`, or `UnknownHostException` from the wrapper: not cached, cannot be fetched. Say so; check your code with `javac` and the check script instead.

**Checking your work.** One scratch file, `Check.java`. Edit it; never re-type it.
- Build inputs in code.
- Copy expected values from the statement, never work them out yourself.
- Print one line per case, never throw: one run shows every case.

```java
import java.util.*;

public class Check {
    public static void main(String[] args) {
        List<List<Integer>> given = List.of(List.of(3, 1, 2));
        List<List<Integer>> want = List.of(List.of(1, 2, 3));
        for (int i = 0; i < given.size(); i++) {
            Object got;
            try { got = Solver.solve(given.get(i)); } catch (Exception e) { got = e.toString(); }
            System.out.println((want.get(i).equals(got) ? "ok " : "BAD") + " " + given.get(i) + " -> " + got + " want " + want.get(i));
        }
    }
}
```

Compile the project, then run `java -cp build/classes/java/main Check.java`. The classpath is the compiled classes: `target/classes` for Maven, `out` for javac. Import the class under test (`import com.acme.Solver;`).
- After any fix, run every case again, passing ones too. One fix can break another case.
- A case fails: check the case before the code. Re-derive it from the statement. A case that breaks a stated rule or holds a value you computed is bad: fix it first.
- Keep the file while the work can change. Delete it when the user is done.

## 3. Code that runs

Compare strings and boxed numbers with `equals`:

```java
if (name == "admin")             // bad: compares references
if ("admin".equals(name))        // good; Objects.equals(a, b) when both may be null
```

Integer arithmetic truncates and overflows:

```java
double avg = sum / count;                  // bad: integer division, 7 / 2 == 3
double avg = (double) sum / count;         // good
int total = price * qty;                   // bad: overflows past 2147483647
long total = (long) price * qty;           // good; true modulo: Math.floorMod(a, n)
```

Throw the exact unchecked exception and message the statement names. Unchecked exceptions need no `throws`:

```java
throw new Exception("bad width");                                // bad: checked, wrong type
throw new IllegalArgumentException("width must be positive");    // good
```

`equals` and `hashCode` go together, from the same fields:

```java
public boolean equals(Point p) { ... }                   // bad: an overload; collections never call it
@Override public boolean equals(Object o) { ... }        // good, plus @Override public int hashCode()
record Point(int x, int y) {}                            // or a record: both generated
```

Do not modify a collection while iterating over it:

```java
for (String s : list) if (s.isEmpty()) list.remove(s);   // bad: ConcurrentModificationException
list.removeIf(String::isEmpty);                          // good
```

- `List.of`, `Arrays.asList` and `stream().toList()` are fixed. Copy with `new ArrayList<>(xs)` before adding or removing.
- `list.remove(1)` on a `List<Integer>` removes index 1. `list.remove(Integer.valueOf(1))` removes the value.
- `int n = map.get(k)` throws `NullPointerException` for a missing key. Use `map.getOrDefault(k, 0)`. Counting: `map.merge(k, 1, Integer::sum)`.
- Print arrays with `Arrays.toString(a)`, compare them with `Arrays.equals(a, b)`.

## 4. Errors

Fix the first compile error first; one broken declaration causes many follow-ups.

| Error | Usual cause and fix |
|---|---|
| `cannot find symbol ... class Foo` / `Foo cannot be resolved to a type` | add the import (`java.util` is not automatic); fix the name or package |
| `cannot find symbol ... method foo(int)` / `is undefined for the type` | wrong name, argument types or receiver; record accessors are `p.x()` |
| `cannot find symbol ... variable x` / `x cannot be resolved` | declare it or fix the spelling or scope |
| `package com.x does not exist` / `The import cannot be resolved` | wrong package, or not in the build (offline: cannot be added) |
| `incompatible types` / `Type mismatch`, `possible lossy conversion from double to int` | convert explicitly or fix the declared type |
| `cannot be applied to given types`, `The constructor is undefined` | match `required:` (order, count, types) |
| `unreported exception X; must be caught or declared to be thrown` | `try`/`catch`, or `throws` on the method; in a lambda wrap it in an unchecked exception |
| `local variables referenced from a lambda expression must be final or effectively final` | do not reassign it; copy to a new variable |
| `missing return statement` | return on every path, or throw at the end |
| `';' expected`, `reached end of file while parsing`, `illegal start of expression` / `Syntax error` | a half-edited block: count braces and parentheses around that line |
| `class Foo is public, should be declared in a file named Foo.java` | the file name must match the public class |
| `The type java.lang.Object cannot be resolved`, `build path is incomplete` | jdtls has no classpath yet: trust the build's compiler |
| `NullPointerException: ... because "<expr>" is null` | fix where that value is set (constructor, missing map key), not at the crash |
| `UnsupportedOperationException` | modifying `List.of` or `Arrays.asList`: copy first |
| `ConcurrentModificationException` | changed during iteration: `removeIf`, or collect changes and apply after the loop |
| `Unsupported class file major version 65`, `UnsupportedClassVersionError` | JDK and build disagree (61 = 17, 65 = 21): set `JAVA_HOME` to another installed JDK |
