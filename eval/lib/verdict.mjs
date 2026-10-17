// A benchmark attempt's verdict, shown in the app: a `verdict` note in the task's session (never
// shown to the model: host_message false) and the session title ending in ` ✓` or ` ✗ passed/total`,
// which the sidebar colours. The hidden tests decide; the app itself never learns the verdict.

/** Passed and total test counts from a test runner's output (Jest/Vitest, pytest, Go, cargo,
 * Gradle/JUnit); null when the output names none. */
export function testCounts(text) {
  const out = String(text ?? '');
  let m;
  if ((m = /Tests:\s+(?:(\d+) failed, )?(?:(\d+) skipped, )?(\d+) passed, (\d+) total/.exec(out))) return { passed: Number(m[3]), total: Number(m[4]) };
  if ((m = /Tests:\s+(\d+) failed, (\d+) total/.exec(out))) return { passed: 0, total: Number(m[2]) };
  const failed = Number((/(\d+) failed/.exec(out) ?? [])[1] ?? 0);
  const passed = Number((/(\d+) passed/.exec(out) ?? [])[1] ?? 0);
  // cargo prints one `test result` line per test binary (the library's own, often empty, first).
  const cargo = [...out.matchAll(/test result: \w+\. (\d+) passed; (\d+) failed/g)];
  if (cargo.length) {
    const p = cargo.reduce((n, r) => n + Number(r[1]), 0);
    const f = cargo.reduce((n, r) => n + Number(r[2]), 0);
    return { passed: p, total: p + f };
  }
  if ((m = /(\d+) tests completed, (\d+) failed/.exec(out))) return { passed: Number(m[1]) - Number(m[2]), total: Number(m[1]) };
  if (passed + failed > 0) return { passed, total: passed + failed };
  const goPass = (out.match(/^\s*--- PASS:/gm) ?? []).length;
  const goFail = (out.match(/^\s*--- FAIL:/gm) ?? []).length;
  if (goPass + goFail > 0) return { passed: goPass, total: goPass + goFail };
  return null;
}

/** Writes the verdict of `attempt` into the session: the note and the title. Never throws. */
export async function markVerdict(client, { sessionId, taskId, attempt, passed, output, secs, final }) {
  if (!client || !sessionId) return;
  const counts = testCounts(output);
  const tally = counts ? `${counts.passed}/${counts.total}` : '';
  const text = passed
    ? `Benchmark passed ✓ (attempt ${attempt}${tally ? `, ${tally} tests` : ''}${secs ? `, ${Math.round(secs)} s` : ''})`
    : `Benchmark failed ✗ (attempt ${attempt}${tally ? `, ${tally} tests` : ''}${final ? '' : '; next attempt follows'})`;
  const data = {
    purpose: 'verdict', host_message: false, text, passed: !!passed, attempt,
    tests_passed: counts?.passed ?? null, tests_total: counts?.total ?? null, secs: secs ?? null, final: !!final,
  };
  await client.request('frame.journal.append', { session_id: sessionId, kind: 'note', data }).catch(() => {});
  const title = passed ? `${taskId} ✓${attempt > 1 ? ` (attempt ${attempt})` : ''}` : `${taskId} ✗${tally ? ` ${tally}` : ''}${final ? '' : ' …'}`;
  await client.request('frame.session.update', { id: sessionId, title }).catch(() => {});
}
