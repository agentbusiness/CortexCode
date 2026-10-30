Java here: get it right, build and test it, code that runs, compiler and runtime errors and fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request and the code it changes (signatures, Javadoc) and its tests. Callers only when a signature changes; then write.
   - Per method or class: what it takes, returns and throws (exception type and message).
   - The request and the project decide, never a name alone. "whether" returns a `boolean`. "find" may return `null` or an `Optional`, as the project does. "returns the sorted list" means a new list, not a sorted argument.
   - When there may be no answer and nothing names a value for it, the declared return type decides: `Optional<T>` gives `Optional.empty()`, an object type gives `null`. Not `-1` or an empty collection unless the project does that.
   - Keep existing class names, packages and signatures.
   - The request and the project disagree: say so and ask, instead of silently picking one.
2. **Before coding, note the request's examples: they are the check, their values copied, never computed.**
   - Each behaviour, value, error or limit the request names is one case. A shown requirement list is that list.
   - Unsure of a domain, format, protocol or library? Look it up: the research plugin, the library's docs, how others solved it:
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
3. **Write whole methods and classes, in slices.**
   - First the simplest complete path (one case, input to result). Compile, run the tests.
   - Add the next behaviour. Run again after every change.
   - A new class is written whole with `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a method: replace it from its signature line to its closing `}`. Never leave part of the old body or a stray brace.
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests (section 2); fix the first failure's cause, run again.
5. **Before you finish**, check each requirement against the code, including the errors the request or the project names.

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- Objects are shared by reference: a returned stored list lets the caller change it. Return `List.copyOf(x)` or `Collections.unmodifiableList(x)` when it must not.
- Values that must not change: `record`, or `final` fields set in the constructor. A map key never changes after it is put.
- Shared between threads: `java.util.concurrent` types (`ConcurrentHashMap`, `AtomicInteger`) or `synchronized` on one lock object.
- Containers: `HashMap` for lookup (`TreeMap` sorted, `LinkedHashMap` in insertion order), `ArrayDeque` for both ends, `PriorityQueue` for smallest first, `HashSet` for uniqueness.
- Standard building blocks first: `java.util` collections with the `Collections` and `Arrays` helpers, `java.time`, `String.join` and `String.format`, streams, before writing your own.

## 2. Run, test, check

Use the project's wrapper when it exists (`./gradlew`, `./mvnw`; `Permission denied`: `chmod +x gradlew`). It downloads what the build needs on first use.

No Java (`Unable to locate a Java Runtime`, `java: command not found`): install it with `brew install openjdk`, run with `permission` saying why (no cask: a cask needs the user's password). Then `export JAVA_HOME=/opt/homebrew/opt/openjdk PATH="/opt/homebrew/opt/openjdk/bin:$PATH"`.

```bash
./gradlew classes -q                                              # compile main classes
./gradlew classes testClasses -q                                  # compile with tests
./gradlew test --tests 'com.acme.FooTest'                         # one class
./gradlew test --tests 'com.acme.FooTest.parsesEmpty'             # one method
mvn -q compile                                                    # Maven compile (./mvnw the same)
mvn -q test-compile                                               # Maven compile with tests
mvn -q test -Dtest='FooTest#parsesEmpty'                          # Maven one method
javac -d out $(find src/main/java -name '*.java')                 # no build tool
java Main.java                                                    # run one source file, no compile step
```

- `> Task :test UP-TO-DATE`: nothing ran; add `--rerun`.
- Gradle prints only the failing test and line. The full message is in `build/test-results/test/TEST-<class>.xml`.
- `Operation not permitted` under `~/.gradle` or `~/.m2`: the caches are outside the write sandbox. Rerun the same command with `permission` saying why (writing Gradle's or Maven's cache).
- `UnknownHostException` from the wrapper: no network right now; say so, and compile with `javac` meanwhile.
- A new dependency in `build.gradle` or `pom.xml` is a change to the user's project: ask before adding one.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: in the build's test folder (`src/test/java` for Gradle or Maven, JUnit as the build declares it), else a class with a `main` in `tests/`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `Collections.nCopies(18, 0)`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `if (!got.equals(want)) System.out.println(got);`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/`; git ignores `tmp/`.

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

Throw the unchecked exception and message the request or the project's API names. Unchecked exceptions need no `throws`:

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
| `package com.x does not exist` / `The import cannot be resolved` | wrong package, or not in the build (adding a dependency: ask the user first) |
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
