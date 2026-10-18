//! A failing test run's output in the form the model acts on: the counts, one line per failing
//! test with its error, and the first failure whole. A runner prints every failure as a traceback
//! (seen: twelve tracebacks, 160 lines, four runs in a row; the model cut them with `| tail`, lost
//! the rest and doubted the file on disk), while a line per case was what it fixed from. Recognised
//! by the runner's own format: unittest, pytest, cargo test, go test, jest or vitest, and TAP
//! (node:test). Any other output, and a passing run, is shown as it is.

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

/// What a runner reported: the failing tests, its own count line, and the first failure's block.
struct Report {
    failures: Vec<Failure>,
    counts: String,
    first: String,
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
}

/// The counts of a test run from its summary line, per runner. `None` when `text` is not a test
/// runner's report.
pub fn summary(text: &str) -> Option<Tests> {
    static UNITTEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Ran (\d+) tests?\b[\s\S]*?^(OK|FAILED \(([^)]*)\))\s*$").expect("valid"));
    static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w+)=(\d+)").expect("valid"));
    static PYTEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^=+ (.*\b(?:passed|failed|errors?)\b.*) in [\d.]+s =+$").expect("valid"));
    static PYTEST_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) (passed|failed|errors?|skipped|xfailed|xpassed)").expect("valid"));
    static CARGO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test result: \w+\. (\d+) passed; (\d+) failed").expect("valid"));
    static GO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(ok|FAIL)\s+\S+\s").expect("valid"));
    static GO_FAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*--- FAIL: (\S+)").expect("valid"));
    static GO_PASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*--- PASS: ").expect("valid"));
    static JEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*Tests:\s+(.*)$").expect("valid"));
    static JEST_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) (failed|passed|total|skipped|todo)").expect("valid"));
    static TAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^# (tests|pass|fail) (\d+)$").expect("valid"));
    let num = |s: &str| s.parse::<usize>().unwrap_or(0);
    let names = |report: Option<Report>| report.map(|report| report.failures.into_iter().map(|failure| failure.name).collect()).unwrap_or_default();
    if let Some(found) = UNITTEST.captures(text) {
        let ran = num(&found[1]);
        let failed: usize = found.get(3).map(|counts| COUNT.captures_iter(counts.as_str()).map(|c| num(&c[2])).sum()).unwrap_or(0);
        return Some(Tests { runner: "unittest", ran, failed, names: names(unittest(text)) });
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
        return Some(Tests { runner: "pytest", ran, failed, names: names(pytest(text)) });
    }
    // cargo prints one `test result:` per test target (unit tests, each tests/ file, doc-tests):
    // the run is their sum, not the first (seen: an empty unit-test target read as "0 of 0 failed"
    // while a tests/ file failed).
    if CARGO.is_match(text) {
        let (passed, failed) = CARGO.captures_iter(text).fold((0, 0), |(passed, failed), found| (passed + num(&found[1]), failed + num(&found[2])));
        return Some(Tests { runner: "cargo", ran: passed + failed, failed, names: names(cargo(text)) });
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
        return Some(Tests { runner: "jest", ran: total, failed, names: names(jest(text)) });
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
        return Some(Tests { runner: "tap", ran, failed, names: names(tap(text)) });
    }
    if GO.is_match(text) || GO_FAIL.is_match(text) {
        let failed = GO_FAIL.captures_iter(text).count();
        let passed = GO_PASS.find_iter(text).count();
        return Some(Tests { runner: "go", ran: passed + failed, failed, names: names(gotest(text)) });
    }
    None
}

/// The compact form of a failing test run, or `None` when `text` is not a recognised runner's
/// report with failures.
pub fn present(text: &str) -> Option<String> {
    let report = unittest(text).or_else(|| pytest(text)).or_else(|| cargo(text)).or_else(|| gotest(text)).or_else(|| jest(text)).or_else(|| tap(text))?;
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
    out.push_str("\nThe first failure, whole:\n");
    // A runner's rule line (`====`) that opens the next block is not part of this one.
    out.push_str(report.first.trim_end().trim_end_matches(['=', '-']).trim_end());
    out.push('\n');
    Some(out)
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
    Some(Report { failures, counts: format!("Ran {} tests: {}", &ran[1], result), first })
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
    Some(Report { failures, counts: summary[1].to_owned(), first })
}

/// cargo test: `test name ... FAILED` lines, `---- name stdout ----` blocks with the panic message,
/// and `test result: FAILED. 3 passed; 2 failed; ...`.
fn cargo(text: &str) -> Option<Report> {
    static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test result: (\w+)\. (.*?)(?:; finished in .*)?$").expect("valid"));
    static FAILED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^test (\S+) \.\.\. FAILED$").expect("valid"));
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^---- (\S+) stdout ----$").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^---- \S+ stdout ----$|^failures:$|^test result:").expect("valid"));
    let result = RESULT.captures(text)?;
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
    let verdict = if sums[1] > 0 { "FAILED" } else { &result[1] };
    Some(Report { failures, counts: format!("test result: {verdict}. {} passed; {} failed; {} ignored", sums[0], sums[1], sums[2]), first })
}

/// go test: `--- FAIL: TestName (0.00s)` with its indented `file.go:12: message` lines, `--- PASS`
/// lines with -v, and the package's `FAIL`/`ok` line.
fn gotest(text: &str) -> Option<Report> {
    static FAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*--- FAIL: (\S+)").expect("valid"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(---|===) |^(FAIL|ok|PASS)\b").expect("valid"));
    static MESSAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+\S+\.go:\d+: (.*)$").expect("valid"));
    let lines: Vec<&str> = text.lines().collect();
    let mut failures = Vec::new();
    let mut first = String::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = FAIL.captures(line) else { continue };
        let body = block(&lines, index, &END);
        if first.is_empty() {
            first = body.join("\n");
        }
        let error = body.iter().find_map(|line| MESSAGE.captures(line).map(|found| found[1].to_owned())).unwrap_or_else(|| last_error(&body));
        failures.push(Failure { name: found[1].to_owned(), status: "FAIL", error });
    }
    if failures.is_empty() {
        return None;
    }
    let passed = lines.iter().filter(|line| line.trim_start().starts_with("--- PASS:")).count();
    let counts = if passed > 0 { format!("{} failed, {} passed", failures.len(), passed) } else { format!("{} failed", failures.len()) };
    Some(Report { failures, counts, first })
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
    Some(Report { failures, counts: format!("Tests: {}", &tests[1]), first })
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
    Some(Report { failures, counts, first })
}
