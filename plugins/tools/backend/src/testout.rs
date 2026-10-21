//! A failing test run's output in the form the model acts on: the counts, one line per failing
//! test with its error, and the first failure whole. A runner prints every failure as a traceback
//! (seen: twelve tracebacks, 160 lines, four runs in a row; the model cut them with `| tail`, lost
//! the rest and doubted the file on disk), while a line per case was what it fixed from. Recognised
//! by the runner's own format: unittest, pytest, cargo test, go test, jest or vitest, TAP
//! (node:test), and JUnit as Gradle and Maven print it. Any other output, and a passing run, is
//! shown as it is.

use regex::Regex;
use std::sync::LazyLock;

/// Failing tests listed before "and N more".
const LISTED: usize = 40;

/// A failing test: its name, how it failed, and its error line.
struct Failure {
    name: String,
    status: &'static str,
    error: String,
}

/// What a runner reported: the failing tests, its own count line, the first failure's block, and
/// what a failure means when the runner's own words hide it (see [`notes`]).
struct Report {
    failures: Vec<Failure>,
    counts: String,
    first: String,
    notes: Vec<String>,
}

/// What a recognised test run reported, passing or failing: the runner, how many tests ran, how
/// many failed, and the failing tests' names. The host reads it from `display.tests` (a green run
/// of the project's tests resets its stuck counters; a probe or an install is not a test run).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Tests {
    pub runner: &'static str,
    pub ran: usize,
    pub failed: usize,
    pub names: Vec<String>,
    /// Tests the runner found but could not run, as `path:line` (see [`never_run`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub never_run: Vec<String>,
}

/// The counts of a test run from its summary line, per runner. `None` when `text` is not a test
/// runner's report.
pub fn summary(text: &str) -> Option<Tests> {
    static UNITTEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Ran (\d+) tests?\b[\s\S]*?^(OK|FAILED \(([^)]*)\))\s*$").expect("valid"));
    static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w+)=(\d+)").expect("valid"));
    static PYTEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^=+ (.*\b(?:passed|failed|errors?)\b.*) in [\d.]+s =+$").expect("valid"));
    static PYTEST_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) (passed|failed|errors?|skipped|xfailed|xpassed)").expect("valid"));
    static CARGO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test result: \w+\. (\d+) passed; (\d+) failed").expect("valid"));
    // A package line as `go test` prints it: `ok`/`FAIL`, the package, then a time or `(cached)`
    // (seen: a node script's `ok ...` lines read as a Go report).
    static GO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(ok|FAIL)\s+\S+\s+(?:[\d.]+s|\(cached\))").expect("valid"));
    static GO_FAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*--- FAIL: (\S+)").expect("valid"));
    static GO_PASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*--- PASS: ").expect("valid"));
    // A package that passed and had tests (`[no tests to run]` marks one whose filter matched none).
    static GO_OK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^ok\s+\S+\s+(?:[\d.]+s|\(cached\))(?:\s+coverage:[^\[\n]*)?\s*$").expect("valid"));
    static JEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*Tests:\s+(.*)$").expect("valid"));
    static JEST_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) (failed|passed|total|skipped|todo)").expect("valid"));
    static TAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^# (tests|pass|fail) (\d+)$").expect("valid"));
    let num = |s: &str| s.parse::<usize>().unwrap_or(0);
    let names = |report: Option<Report>| report.map(|report| report.failures.into_iter().map(|failure| failure.name).collect()).unwrap_or_default();
    if let Some(found) = UNITTEST.captures(text) {
        let ran = num(&found[1]);
        let failed: usize = found.get(3).map(|counts| COUNT.captures_iter(counts.as_str()).map(|c| num(&c[2])).sum()).unwrap_or(0);
        return Some(Tests { runner: "unittest", ran, failed, names: names(unittest(text)), never_run: Vec::new() });
    }
    if let Some(found) = PYTEST.captures(text) {
        let mut ran = 0;
        let mut failed = 0;
        for c in PYTEST_N.captures_iter(&found[1]) {
            let n = num(&c[1]);
            ran += n;
            if c[2].starts_with("failed") || c[2].starts_with("error") {
                failed += n;
            }
        }
        return Some(Tests { runner: "pytest", ran, failed, names: names(pytest(text)), never_run: Vec::new() });
    }
    // cargo prints one `test result:` per test target (unit tests, each tests/ file, doc-tests):
    // the run is their sum, not the first (seen: an empty unit-test target read as "0 of 0 failed"
    // while a tests/ file failed).
    // A test program that died (a signal) reports no line at all: it counts as a failure.
    let crashed = cargo_crashes(text).len();
    if CARGO.is_match(text) || crashed > 0 {
        let (passed, failed) = CARGO.captures_iter(text).fold((0, 0), |(passed, failed), found| (passed + num(&found[1]), failed + num(&found[2])));
        return Some(Tests { runner: "cargo", ran: passed + failed + crashed, failed: failed + crashed, names: names(cargo(text)), never_run: never_run(text) });
    }
    if let Some(found) = JEST.captures(text) {
        let mut total = 0;
        let mut failed = 0;
        for c in JEST_N.captures_iter(&found[1]) {
            match &c[2] {
                "total" => total = num(&c[1]),
                "failed" => failed = num(&c[1]),
                _ => {}
            }
        }
        return Some(Tests { runner: "jest", ran: total, failed, names: names(jest(text)), never_run: Vec::new() });
    }
    if TAP.is_match(text) {
        let mut ran = 0;
        let mut failed = 0;
        for c in TAP.captures_iter(text) {
            match &c[1] {
                "tests" => ran = num(&c[2]),
                "fail" => failed = num(&c[2]),
                _ => {}
            }
        }
        return Some(Tests { runner: "tap", ran, failed, names: names(tap(text)), never_run: Vec::new() });
    }
    if GO.is_match(text) || GO_FAIL.is_match(text) {
        let failed = GO_FAIL.captures_iter(text).count();
        let passed = GO_PASS.find_iter(text).count();
        // Without -v a passing package prints only `ok  pkg  0.3s`: its tests ran, uncounted. Each
        // such package counts as one (seen: a green run read as "no tests ran").
        let quiet = if passed + failed == 0 { GO_OK.find_iter(text).count() } else { 0 };
        return Some(Tests { runner: "go", ran: passed + failed + quiet, failed, names: names(gotest(text)), never_run: Vec::new() });
    }
    if let Some((ran, failed)) = junit_counts(text) {
        return Some(Tests { runner: "junit", ran, failed, names: names(junit(text)), never_run: Vec::new() });
    }
    None
}

/// Tests the runner found but cannot run, as `path:line`: cargo's `#[test]` functions inside another
/// function (`warning: cannot test inner items`, a `}` missing above them). A run with these passes
/// without them (seen: two tests of the case being fixed never ran, the run read "0 of 8 failed" and
/// the fix was taken as done).
pub fn never_run(text: &str) -> Vec<String> {
    static INNER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^warning: cannot test inner items\s*\n\s*--> (\S+?):(\d+):\d+").expect("valid"));
    let mut spots: Vec<String> = INNER.captures_iter(text).map(|found| format!("{}:{}", &found[1], &found[2])).collect();
    spots.sort();
    spots.dedup();
    spots
}

/// Whether a runner reported that it found no tests to run (jest, vitest, pytest, unittest,
/// go test, Gradle, node:test, mocha, cargo). A name filter that matched nothing is not this:
/// the tests exist.
pub fn found_none(text: &str) -> bool {
    static NONE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?mi)No tests found|No test files found|\bno tests ran\b|\[no test files\]|:test NO-SOURCE|^# tests 0$|^\s*0 passing\b").expect("valid")
    });
    static FILTERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[1-9]\d* filtered out").expect("valid"));
    NONE.is_match(text) || (!FILTERED.is_match(text) && summary(text).is_some_and(|tests| tests.ran == 0))
}

/// The compact form of a failing test run, or `None` when `text` is not a recognised runner's
/// report with failures.
pub fn present(text: &str) -> Option<String> {
    let report = unittest(text).or_else(|| pytest(text)).or_else(|| cargo(text)).or_else(|| gotest(text)).or_else(|| jest(text)).or_else(|| tap(text)).or_else(|| junit(text))?;
    if report.failures.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str(report.counts.trim());
    out.push('\n');
    for failure in report.failures.iter().take(LISTED) {
        out.push_str(&format!("{} {}: {}\n", failure.status, failure.name, failure.error.trim()));
    }
    if report.failures.len() > LISTED {
        out.push_str(&format!("and {} more\n", report.failures.len() - LISTED));
    }
    for note in &report.notes {
        out.push_str(note);
        out.push('\n');
    }
    // A runner's rule line (`====`) that opens the next block is not part of this one.
    let first = report.first.trim_end().trim_end_matches(['=', '-']).trim_end();
    // No block to show (a crashed test program prints none): no heading over nothing.
    if !first.trim().is_empty() {
        out.push_str("\nThe first failure, whole:\n");
        out.push_str(first);
        out.push('\n');
    }
    Some(out)
}

/// What a failure means where the runner's own words hide it, one line each, for any runner's output
/// (also output shown whole, which gets no compact form).
pub fn notes(text: &str) -> Vec<String> {
    jest(text).map(|report| report.notes).unwrap_or_default()
}

/// The lines of `text` from `start` up to the first later line matching `end` (exclusive).
fn block<'a>(lines: &[&'a str], start: usize, end: &Regex) -> Vec<&'a str> {
    let stop = lines.iter().enumerate().skip(start + 1).find(|(_, line)| end.is_match(line)).map(|(index, _)| index).unwrap_or(lines.len());
    lines[start..stop].to_vec()
}

/// The last line of a block that reads as an error (`AssertionError: 28 != 19`), else its last
/// non-empty line.
fn last_error(lines: &[&str]) -> String {
    static ERROR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[A-Za-z_][\w.]*(Error|Exception|Exit|Failure)\b.*|^\s*E\s{2,}\S.*|^\s*(error|panicked|assertion)\b.*").expect("valid"));
    let fallback = lines.iter().rev().find(|line| !line.trim().is_empty() && !line.trim().chars().all(|c| c == '-' || c == '=')).map(|line| line.trim().to_owned()).unwrap_or_default();
    lines.iter().rev().find(|line| ERROR.is_match(line)).map(|line| line.trim().to_owned()).unwrap_or(fallback)
}

/// Python unittest: `FAIL: name (module.Class.name)` and `ERROR:` blocks, `Ran N tests`, `FAILED
/// (failures=a, errors=b)`.
fn unittest(text: &str) -> Option<Report> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(FAIL|ERROR): (\S+)").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(FAIL|ERROR): \S+|^Ran \d+ tests?\b").expect("valid"));
    static RAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Ran (\d+) tests?\b").expect("valid"));
    static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(OK|FAILED \([^)]*\))\s*$").expect("valid"));
    let ran = RAN.captures(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let mut failures = Vec::new();
    let mut first = String::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = HEAD.captures(line) else { continue };
        // The head, its `----` rule and the traceback, up to the next head or the `Ran` line.
        let body = block(&lines, index, &END);
        if first.is_empty() {
            first = body.join("\n");
        }
        let status = if &found[1] == "FAIL" { "FAIL" } else { "ERROR" };
        failures.push(Failure { name: found[2].to_owned(), status, error: last_error(&body[1..]) });
    }
    let result = RESULT.captures(text).map(|found| found[1].to_owned()).unwrap_or_default();
    Some(Report { failures, counts: format!("Ran {} tests: {}", &ran[1], result), first, notes: Vec::new() })
}

/// pytest: `FAILED path::name - Error` summary lines, `____ name ____` blocks with `E   ` lines,
/// and the `=== 3 failed, 10 passed in 0.1s ===` line.
fn pytest(text: &str) -> Option<Report> {
    static SUMMARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^=+ (.*\b(passed|failed|errors?)\b.* in [\d.]+s) =+$").expect("valid"));
    static SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(FAILED|ERROR) (\S+)(?: - (.*))?$").expect("valid"));
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^_{3,} (.+?) _{3,}$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^_{3,} .+? _{3,}$|^={3,} .* ={3,}$|^-{3,} .* -{3,}$").expect("valid"));
    static E_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^E\s{2,}(\S.*)$").expect("valid"));
    let summary = SUMMARY.captures(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<(String, Vec<&str>)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = BLOCK.captures(line) {
            blocks.push((found[1].to_owned(), block(&lines, index, &END)));
        }
    }
    let mut failures: Vec<Failure> = SHORT
        .captures_iter(text)
        .map(|found| Failure { name: found[2].to_owned(), status: if &found[1] == "FAILED" { "FAIL" } else { "ERROR" }, error: found.get(3).map(|m| m.as_str().to_owned()).unwrap_or_default() })
        .collect();
    if failures.is_empty() {
        failures = blocks
            .iter()
            .map(|(name, body)| Failure { name: name.clone(), status: "FAIL", error: body.iter().find_map(|line| E_LINE.captures(line).map(|found| found[1].to_owned())).unwrap_or_else(|| last_error(body)) })
            .collect();
    }
    for failure in &mut failures {
        if failure.error.is_empty()
            && let Some((_, body)) = blocks.iter().find(|(name, _)| failure.name.ends_with(name.as_str()) || name.ends_with(failure.name.rsplit("::").next().unwrap_or("")))
        {
            failure.error = body.iter().find_map(|line| E_LINE.captures(line).map(|found| found[1].to_owned())).unwrap_or_else(|| last_error(body));
        }
    }
    let first = blocks.first().map(|(_, body)| body.join("\n")).unwrap_or_default();
    Some(Report { failures, counts: summary[1].to_owned(), first, notes: Vec::new() })
}

/// cargo test: `test name ... FAILED` lines, `---- name stdout ----` blocks with the panic message,
/// and `test result: FAILED. 3 passed; 2 failed; ...`.
fn cargo(text: &str) -> Option<Report> {
    static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test result: (\w+)\. (.*?)(?:; finished in .*)?$").expect("valid"));
    static FAILED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test (\S+) \.\.\. FAILED$").expect("valid"));
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^---- (\S+) stdout ----$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^---- \S+ stdout ----$|^failures:$|^test result:").expect("valid"));
    let result = RESULT.captures(text);
    let crashes = cargo_crashes(text);
    if result.is_none() && crashes.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<(String, Vec<&str>)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = BLOCK.captures(line) {
            blocks.push((found[1].to_owned(), block(&lines, index, &END)));
        }
    }
    let message = |body: &[&str]| -> String {
        // `thread 'x' panicked at src/a.rs:1:2:` is followed by the message line; an assertion's
        // `left`/`right` lines come after it.
        let at = body.iter().position(|line| line.contains("panicked at"));
        match at {
            Some(at) => body[at + 1..].iter().filter(|line| !line.trim().is_empty()).take(3).map(|line| line.trim()).collect::<Vec<_>>().join(" "),
            None => last_error(body),
        }
    };
    let mut failures: Vec<Failure> = FAILED
        .captures_iter(text)
        .map(|found| {
            let name = found[1].to_owned();
            let error = blocks.iter().find(|(block_name, _)| *block_name == name).map(|(_, body)| message(body)).unwrap_or_default();
            Failure { name, status: "FAIL", error }
        })
        .collect();
    if failures.is_empty() {
        failures = blocks.iter().map(|(name, body)| Failure { name: name.clone(), status: "FAIL", error: message(body) }).collect();
    }
    let crashed = crashes.len();
    failures.extend(crashes);
    let first = blocks.first().map(|(_, body)| body.join("\n")).unwrap_or_default();
    // Every target's counts summed (cargo prints one `test result:` per target).
    static COUNTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) (passed|failed|ignored)").expect("valid"));
    let mut sums = [0usize; 3];
    for line in RESULT.captures_iter(text) {
        for found in COUNTS.captures_iter(&line[2]) {
            let index = match &found[2] { "passed" => 0, "failed" => 1, _ => 2 };
            sums[index] += found[1].parse::<usize>().unwrap_or(0);
        }
    }
    let verdict = if sums[1] > 0 || crashed > 0 { "FAILED" } else { result.as_ref().map_or("ok", |found| found.get(1).map_or("ok", |m| m.as_str())) };
    let mut counts = format!("test result: {verdict}. {} passed; {} failed; {} ignored", sums[0], sums[1], sums[2]);
    if crashed > 0 {
        counts.push_str(&format!("; {crashed} test program{} crashed", if crashed == 1 { "" } else { "s" }));
    }
    Some(Report { failures, counts, first, notes: Vec::new() })
}

/// cargo's test programs that died before reporting (a signal: a trap, an abort, a segfault): no
/// `test result:` line counts their tests (seen: a SIGTRAP read as "0 of 0 failed" because the
/// library's empty target reported `ok`). One failure per program, named after it.
fn cargo_crashes(text: &str) -> Vec<Failure> {
    static DIED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"process didn't exit successfully: `([^`]+)` \(signal: \d+, (SIG\w+)").expect("valid"));
    static HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-[0-9a-f]{16}$").expect("valid"));
    DIED.captures_iter(text)
        .map(|found| {
            let program = found[1].split_whitespace().next().unwrap_or("");
            let program = program.rsplit('/').next().unwrap_or(program);
            Failure {
                name: HASH.replace(program, "").into_owned(),
                status: "CRASH",
                error: format!("the test program died ({}) before it reported its tests; run it alone with `-- --test-threads=1 --nocapture` to see which test was running", &found[2]),
            }
        })
        .collect()
}

/// go test: `--- FAIL: TestName (0.00s)` with its `file.go:12: message` lines, `--- PASS` lines with
/// -v, and the package's `FAIL`/`ok` line. With -v a test's own lines come between its `=== RUN`
/// line and its `--- FAIL` line; without it (and for subtests) they follow the FAIL line, indented
/// (seen: with -v every failure shown without its reason, and the same tests run again).
fn gotest(text: &str) -> Option<Report> {
    static FAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*--- FAIL: (\S+)").expect("valid"));
    static RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*=== RUN\s+(\S+)\s*$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(---|===) |^(FAIL|ok|PASS)\b").expect("valid"));
    static MESSAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+\S+\.go:\d+: (.*)$").expect("valid"));
    let lines: Vec<&str> = text.lines().collect();
    let mut failures = Vec::new();
    let mut first = String::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = FAIL.captures(line) else { continue };
        let name = &found[1];
        // The test's own lines before its FAIL line (-v): from its `=== RUN` line, without the
        // runner's lines of other tests (`=== RUN`, `=== PAUSE`, `--- PASS` of subtests).
        let run = lines[..index].iter().rposition(|line| RUN.captures(line).is_some_and(|found| &found[1] == name));
        let before: Vec<&str> = run.map(|run| lines[run + 1..index].iter().filter(|line| !END.is_match(line)).copied().collect()).unwrap_or_default();
        let after = block(&lines, index, &END);
        let mut body: Vec<&str> = vec![after[0]];
        body.extend(before);
        body.extend(after.iter().skip(1));
        if first.is_empty() {
            first = body.join("\n");
        }
        let error = body.iter().find_map(|line| MESSAGE.captures(line).map(|found| found[1].to_owned())).unwrap_or_else(|| last_error(&body[1..]));
        failures.push(Failure { name: name.to_owned(), status: "FAIL", error });
    }
    if failures.is_empty() {
        return None;
    }
    let passed = lines.iter().filter(|line| line.trim_start().starts_with("--- PASS:")).count();
    let counts = if passed > 0 { format!("{} failed, {} passed", failures.len(), passed) } else { format!("{} failed", failures.len()) };
    Some(Report { failures, counts, first, notes: Vec::new() })
}

/// jest or vitest: `✕ name` lines, `● Suite › name` blocks, and `Tests: 2 failed, 10 passed, 12 total`.
fn jest(text: &str) -> Option<Report> {
    static TESTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*Tests:\s+(.*)$").expect("valid"));
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*● (.+?)\s*$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*● |^\s*Test Suites:|^\s*Tests:").expect("valid"));
    static CROSS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[✕×✗] (.+?)(?: \(\d+ ?ms\))?\s*$").expect("valid"));
    let tests = TESTS.captures(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<(String, Vec<&str>)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = BLOCK.captures(line) && !found[1].contains("Console") {
            blocks.push((found[1].to_owned(), block(&lines, index, &END)));
        }
    }
    let error_of = |body: &[&str]| -> String {
        body.iter().skip(1).map(|line| line.trim()).find(|line| !line.is_empty() && !line.starts_with('>') && !line.starts_with('|') && !line.starts_with("at ")).unwrap_or("").to_owned()
    };
    let mut failures: Vec<Failure> = CROSS.captures_iter(text).map(|found| Failure { name: found[1].to_owned(), status: "FAIL", error: String::new() }).collect();
    if failures.is_empty() {
        failures = blocks.iter().map(|(name, body)| Failure { name: name.clone(), status: "FAIL", error: error_of(body) }).collect();
    } else {
        for failure in &mut failures {
            if let Some((_, body)) = blocks.iter().find(|(name, _)| name.ends_with(failure.name.as_str())) {
                failure.error = error_of(body);
            }
        }
    }
    let first = blocks.first().map(|(_, body)| body.join("\n")).unwrap_or_default();
    let notes = jest_notes(&blocks);
    Some(Report { failures, counts: format!("Tests: {}", &tests[1]), first, notes })
}

/// TAP (node:test): `not ok N - name` with its YAML diagnostics, and the `# tests`/`# pass`/
/// `# fail` lines.
fn tap(text: &str) -> Option<Report> {
    static NOT_OK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*not ok \d+ - (.+?)\s*(?:#.*)?$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(not )?ok \d+ |^# |^\s*\.\.\.\s*$").expect("valid"));
    static ERROR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s+(?:error|message): '?"?(.*?)'?"?\s*$"#).expect("valid"));
    static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^# (tests|pass|fail) (\d+)$").expect("valid"));
    if !text.contains("not ok ") || !COUNT.is_match(text) {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut failures = Vec::new();
    let mut first = String::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = NOT_OK.captures(line) else { continue };
        let body = block(&lines, index, &END);
        if first.is_empty() {
            first = body.join("\n");
        }
        let error = body.iter().find_map(|line| ERROR.captures(line).map(|found| found[1].to_owned())).unwrap_or_else(|| last_error(&body));
        failures.push(Failure { name: found[1].to_owned(), status: "FAIL", error });
    }
    let counts = COUNT.captures_iter(text).map(|found| format!("{} {}", &found[2], &found[1])).collect::<Vec<_>>().join(", ");
    Some(Report { failures, counts, first, notes: Vec::new() })
}

/// The counts of a JUnit run: Gradle's `17 tests completed, 2 failed` (or, with test events logged,
/// its `Class > test() PASSED|FAILED|SKIPPED` lines), and Maven Surefire's closing `Tests run: 17,
/// Failures: 1, Errors: 1, Skipped: 0` (failures and errors both count as failed).
fn junit_counts(text: &str) -> Option<(usize, usize)> {
    static GRADLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(\d+) tests? completed(?:, (\d+) failed)?").expect("valid"));
    static EVENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\S.*? > .+? (PASSED|FAILED|SKIPPED)\s*$").expect("valid"));
    static MAVEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(?:\[\w+\] )?Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: \d+\s*$").expect("valid"));
    let num = |s: &str| s.parse::<usize>().unwrap_or(0);
    if let Some(found) = GRADLE.captures(text) {
        return Some((num(&found[1]), found.get(2).map_or(0, |m| num(m.as_str()))));
    }
    // Maven prints one line per test class (with `Time elapsed`) and the total last, without it.
    if let Some(found) = MAVEN.captures_iter(text).last() {
        return Some((num(&found[1]), num(&found[2]) + num(&found[3])));
    }
    let events: Vec<String> = EVENT.captures_iter(text).map(|found| found[1].to_owned()).collect();
    if events.is_empty() {
        return None;
    }
    let ran = events.iter().filter(|event| *event != "SKIPPED").count();
    Some((ran, events.iter().filter(|event| *event == "FAILED").count()))
}

/// JUnit as Gradle prints it: `Class > test() FAILED`, then the exception and its message, indented,
/// then the stack frames; and as Maven Surefire prints it: `[ERROR]   Class.test:95 message` under
/// `[ERROR] Failures:` and `[ERROR] Errors:`. One line per failure: the message, and the first frame
/// of the code under test when there is no message (seen: the expected texts buried in an 82 KB log
/// of stack frames, two of them missed).
fn junit(text: &str) -> Option<Report> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\S.*? > .+?) FAILED\s*$").expect("valid"));
    static FRAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+at \S").expect("valid"));
    static SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[ERROR\] (Failures|Errors):\s*$").expect("valid"));
    static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[ERROR\]\s{2,}(\S+?)(?::\d+)?\s+(\S.*)$").expect("valid"));
    let (ran, failed) = junit_counts(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let mut failures = Vec::new();
    let mut first = String::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = HEAD.captures(line) {
            // The block: the indented lines after the head (blank lines inside it included).
            let end = lines.iter().enumerate().skip(index + 1).find(|(_, line)| !line.is_empty() && !line.starts_with(char::is_whitespace)).map_or(lines.len(), |(end, _)| end);
            let body = &lines[index + 1..end];
            // The message: the lines before the stack frames, without the assertion library's
            // "Throwable that failed the check:" tail.
            let message: Vec<&str> = body
                .iter()
                .take_while(|line| !FRAME.is_match(line) && !line.trim_start().starts_with("Throwable that failed the check"))
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect();
            let frame = body.iter().find(|line| FRAME.is_match(line)).map(|line| line.trim().to_owned());
            let error = match (message.len(), &frame) {
                (0 | 1, Some(frame)) => format!("{} ({frame})", message.join(" ")),
                _ => message.join(" "),
            };
            if first.is_empty() {
                // The head, the message and the first few frames: the rest is the test framework's.
                let frames = body.iter().filter(|line| FRAME.is_match(line)).take(3);
                let mut kept: Vec<&str> = vec![line];
                kept.extend(body.iter().take_while(|line| !FRAME.is_match(line)).copied());
                kept.extend(frames.copied());
                first = kept.join("\n");
            }
            failures.push(Failure { name: found[1].to_owned(), status: "FAIL", error: error.trim().to_owned() });
        }
    }
    if failures.is_empty() {
        let mut status = None;
        for line in &lines {
            if let Some(found) = SECTION.captures(line) {
                status = Some(if &found[1] == "Failures" { "FAIL" } else { "ERROR" });
                continue;
            }
            match (status, ITEM.captures(line)) {
                (Some(status), Some(found)) => failures.push(Failure { name: found[1].to_owned(), status, error: found[2].trim().to_owned() }),
                (Some(_), None) if !line.starts_with("[ERROR]   ") => status = None,
                _ => {}
            }
        }
        if let Some(failure) = failures.first() {
            first = format!("{} {}: {}", failure.status, failure.name, failure.error);
        }
    }
    Some(Report { failures, counts: format!("{ran} tests, {failed} failed"), first, notes: Vec::new() })
}

/// Jest's own words that hide what failed: a deep equality that overflowed the stack while printing
/// its difference (seen: `-0` against `0` in an object whose getter returns a new object of its
/// class, reported only as "Maximum call stack size exceeded", and taken for recursion in the code).
fn jest_notes(blocks: &[(String, Vec<&str>)]) -> Vec<String> {
    blocks
        .iter()
        .filter(|(_, body)| body.iter().any(|line| line.contains("Maximum call stack size exceeded")) && body.iter().any(|line| line.contains("toEqual") || line.contains("toStrictEqual")))
        .map(|(name, _)| {
            format!(
                "{name}: the two values differ and Jest could not print them (an object whose getter returns a new object of its class makes the printing recurse). Compare the fields one by one with `Object.is`, which also tells `-0` from `0`."
            )
        })
        .collect()
}
