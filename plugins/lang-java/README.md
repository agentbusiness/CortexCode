# Java (`lang-java`)

Everything CortexCode knows about Java, as one `lang@1` plugin (the `lang` family, see
[docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of `.java`
files, `pom.xml` and `build.gradle(.kts)`, Eclipse JDT LS (jdtls) for diagnostics, definition,
references and hover, a JDK 21 installed into CortexCode's toolchains directory when the system has
none, the Gradle / Maven verifiers, and the Java skills. A `LanguagePack` served by `cc-lang-kit`,
built like [lang-rust](../lang-rust/README.md).

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-java`: category `lang-java`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

- **Languages**: `java` (symbol-id prefix `java`, extension `java`; the memory plugin's names, so
  code-graph ids stay the same), `pom.xml` (prefix `maven`) and `build.gradle` (prefix `gradle`, file
  names `build.gradle` and `build.gradle.kts`).
- **extract**: tree-sitter-java facts, the memory plugin's extractor ported to `lang@1` facts:
  package, imports (static ones bound by member name), classes, interfaces, enums with constants,
  records with components, annotation types, fields and constants with values, methods and
  constructors with parameters and Javadoc, JUnit 3/4/5 and TestNG tests (in Surefire's test files
  or `src/test/`), calls, instantiations, field reads and writes (`this.` and implicit), exceptions
  thrown, typed locals, `System.getenv` reads and programs run through `ProcessBuilder` /
  `Runtime.exec`. pom.xml (XML elements): `groupId:artifactId` (group inherited from `<parent>`),
  dependencies by scope (`test` -> `dev`, `optional`, `provided`, `runtime`, ...), managed
  dependencies and build plugins, `${property}` references resolved from `<properties>` and
  `project.*`, the version marked installed when it is in `~/.m2/repository`. build.gradle(.kts)
  (lexed: comments, strings, brackets): `group`, and every dependency of `dependencies { }` blocks in
  string, map / named-argument, `project(...)`, `platform(...)` and version-catalog (`libs.x`)
  notation (`test*` configurations -> `dev`, `buildscript` -> `build`). The artifact name of a
  Gradle project is in settings.gradle, so `package` is the group only.
- **Java detection** never trusts `/usr/bin/java` (a stub without an installed JDK): `$JAVA_HOME`,
  then `/usr/libexec/java_home` (which fails when there is no JDK), then a `java` on PATH other than
  the stub that lives inside a JDK, each accepted only when `java -version` runs.
- **install**: Eclipse Temurin 21.0.12.1+1 (Adoptium's macOS archive, SHA-256 pinned) into
  `toolchains/jdk/21.0.12.1+1` when there is no JDK 21+, then jdtls 1.61.0 (the
  download.eclipse.org milestone archive, SHA-256 pinned) into `toolchains/jdtls/1.61.0`.
- **Server**: the executable is the JDK's `java` (the installed Temurin, else a system JDK 21+);
  jdtls needs a per-workspace `-data` directory and its launcher jar's versioned name, so the server
  of each root starts (the pack's `server_launch`) with jdtls's own `bin/jdtls` arguments, the
  shared configuration read-only and `-data $CORTEX_HOME/cache/jdtls/<workspace>-<hash>` (created
  then). `[plugin.config] server_path` names another server executable (a `jdtls` script), given
  only `-data <dir>`. `describe` reports jdtls missing, or the JDK problem (none, or too old, with
  its version), with the install hint.
- **Toolchain** (`describe`'s `path` and `env`): the installed JDK's `bin` and `JAVA_HOME` when the
  system has no JDK of its own (Gradle wrappers and Maven need `JAVA_HOME`).
- **Verifiers** (`requires`: the programs the consumer checks on PATH): with `gradlew`:
  `./gradlew classes testClasses --offline -q` (build), `./gradlew test --offline` (test); with
  `mvnw`: `./mvnw -q -o test-compile` (build), `./mvnw -q -o test` (test), all requiring `java`;
  with `pom.xml`: `mvn -q -o test-compile` (build), `mvn -q -o test` (test), requiring `mvn` and
  `java`.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-java
```
