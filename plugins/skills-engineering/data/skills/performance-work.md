Code too slow or memory-hungry, or a time/memory limit to meet: measure and profile; not SQL plans.

## Procedure

1. **Target**: operation, input size, current time or memory, required value.
2. **One measurement command**: realistic (generated if needed) input, optimised build (`--release`, `-O2`), 3 runs, note the spread:

```bash
for i in 1 2 3; do /usr/bin/time -l <cmd> 2>&1 | grep -E 'real|maximum resident'; done   # macOS; Linux: /usr/bin/time -v
```

3. **Profile**; work only on the top entries. Guesses are usually wrong.
4. **Fix in this order**:
   1. algorithm, data structure (a lookup in a loop -> a dict or set built once);
   2. IO pattern (a query, request or commit per row -> one batch);
   3. allocations, copies;
   4. micro-optimisation.
5. **One change at a time**; rerun the same command; keep it only if it clearly wins and tests pass.

## Profile

```bash
python -m cProfile -s cumtime script.py 2>&1 | head -40
rm -f isolate-*.log && node --prof app.js && node --prof-process isolate-*.log | head -40
go test -run '^$' -bench . -cpuprofile /tmp/cpu.out ./pkg && go tool pprof -top -cum /tmp/cpu.out | head -30
java -XX:StartFlightRecording=filename=/tmp/rec.jfr,dumponexit=true -jar app.jar; jfr view hot-methods /tmp/rec.jfr
sample <pid> 5 -file /tmp/s.txt && grep -A25 "Sort by top of stack" /tmp/s.txt   # macOS native code, while it runs
perf record -g ./prog && perf report --stdio --no-children | head -60            # Linux native code (build -O2 -g)
```

- Rust symbols in release: `CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release`.
- Python memory: `tracemalloc.start()` at start, `tracemalloc.take_snapshot().statistics("lineno")[:10]` at the peak.
- No profiler: interrupt it several times in a `terminal` debugger (`<C-c>`, `bt`); the function on most stacks is hot.

## Read the profile

- Cumulative time finds the call path; own time finds the line.
- Time in write or print syscalls: buffer output.
- Time in allocation or GC: reuse buffers, iterate lazily.
- Memory grows with input: stream; bound caches.
- A cache without an invalidation rule is a bug.

## Do not

- Trade correctness for speed: tests pass after every change.
- Leave profiler output (`isolate-*.log`, `perf.data`, `*.prof`, `*.test` binaries) in the repository.
