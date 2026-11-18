# Reproducing parser and resource stress checks

Phase 8 adds deterministic parser properties, historical-schema and concurrent
initializer regressions, a [bounded mutation package](../../tests/fuzz/README.md),
and two opt-in wall-clock resource soaks. Normal workspace tests compile the
soaks and report them ignored; the explicit runner executes them.

From the repository root:

```sh
python3 scripts/resource-soak.py --seconds 180 --output /tmp/soju-resource-soak
```

The runner starts isolated optimized test processes concurrently, with the
requested load duration and five seconds of cooldown. Linux `/proc` supplies RSS
and descriptor samples; Tokio supplies live-task counts. Logs retain command
results and samples every fifteen seconds. Durations accept 30–3,600 seconds;
longer campaigns should use the same recorded inputs and dedicated hosts.

The connection/history process uses mimalloc, a real listener limited to sixteen
physical sessions, repeated sixteen-admitted/four-rejected connection bursts,
CAP exchanges that create and exercise each connection writer before disconnect,
2,000 distinct conversation targets and 256 hot 16-KiB messages per batch. It
checks session release between bursts and bounded history counters throughout.
At completion it deletes history, stops the server, and records cleanup after
five seconds. This isolates accepted-session and memory-store ownership; it
does not simulate authenticated TLS clients, persistent receipts or full IRC
actor traffic.

The push process uses the library test allocator and the production bounded
notification queue/worker. It offers 512 distinct ready jobs per ten-ms tick;
a local HTTP responder takes 250 ms per completion. Only one request is active,
with at most 128 queued conversations and one MiB of accounted pending bytes.
Cancellation joins worker and endpoint, then checks zero active/pending counters.
This measures slow-I/O queue ownership; it does not exercise Web Push encryption,
subscription authentication or the sender-wide HTTP semaphore. Their existing
functional/admission regressions remain in the normal suite.

RSS is sampled independently from accounted history/queue bytes. A fixed
counter plateau and successful cleanup do not imply that an allocator returns
all pages to the OS or that every unmeasured subsystem has a memory bound.
These finite-duration workloads also do not replace a multi-hour deployment
soak or the final Phase 10 compatibility/performance assessment.

## Recorded Phase 8 results

Measured on 2026-11-18 with Rust 1.100.0, Linux x86_64, optimized
builds and two Tokio workers per soak process. The two original workload
processes ran concurrently. A second connection/history run adds CAP exchanges
to ensure real writer creation and teardown; that strengthened run is reported
below. Host build activity overlapped early samples, so these results establish
retention/cancellation behavior for the specified workload rather than throughput.

| Workload | Actual load and operations | Retained state at load samples | Cleanup after five seconds |
| --- | --- | --- | --- |
| CAP connections and sender/history churn | 180 seconds; 1,222 batches; 19,552 admitted and 4,888 rejected connections; 2,756,832 history appends | 1,024 targets, 1,279 messages, 5,101,060–5,102,083 accounted bytes; one task and 11 FDs between bursts | Zero sessions/targets/messages/bytes and tasks; 10 FDs |
| Slow local HTTP push model | 180 seconds; 7,987,200 offered jobs, 716 completed responses, 7,986,356 overflowed admissions | 128 pending conversations, 97,756 pending bytes, one active job, two tasks and 13 FDs | Zero pending/active bytes/jobs and tasks; 10 FDs |

From 60 seconds onward, history-process RSS sampled 29,648–29,784 KiB;
push-process RSS sampled 7,528–7,532 KiB. Drained RSS was
29,856 KiB and 7,788 KiB respectively. Counters, descriptors
and tasks cleaned up; RSS retained allocator pages. The small history-byte
increase follows the additional digit in generated conversation keys. Absolute
RSS is not comparable across the two allocator/process configurations.

The first admission/history run also passed its 180-second load and cleanup;
it retained 1,024 targets/1,279 messages, one task and 11 FDs between bursts.
The strengthened CAP run supplies the writer-teardown evidence.

Parser properties pass 2,048 tag cases, 2,048 message cases, 1,024 multiline
cases, 8,192 duration cases, 12,288 BARE integer cases, 25 network/target ID
combinations with truncated prefixes, and seventeen fragmented UTF-8 schedules.
Seeds and generator loops are checked in. Multiline properties model interior
blank-line preservation, trailing-newline normalization and empty-batch rejection.

All four mutation targets pass 10,000 mutations for each of seeds
`1592598566` and `11400714819323198485`: 80,000 mutations plus seed replays,
including large tag sets and the scfg nesting limit. A four-byte minimized
harness counterexample (` @ x`) is retained in the IRC corpus; the invalid
roundtrip assumption was corrected without changing the permissive parser.
No product crash was found in these campaigns. This is deterministic mutation
smoke, with no coverage-guided completeness claim.

The schema 30–35 migration/search/cursor checks, original older-schema fixture,
transaction/FTS/FK rollback cases and genuine eight-connection initializer races
pass on GNU and musl. The initializer regression exposed a real WAL setup race:
`PRAGMA journal_mode=WAL` could return `SQLITE_BUSY` without invoking the busy
handler. Setup now retries that operation within a five-second busy budget and
restores the existing ordinary-operation timeout. Twenty repeated race tests
passed after the fix, covering 480 concurrent initializer opens.

Final workspace gates pass 298 tests per GNU/musl target plus reader smoke and
doc tests. The two timed tests remain opt-in/ignored in the normal suite and
were executed separately as recorded above. Strict Clippy on both targets,
strict rustdoc, standalone fuzz formatting/Clippy/build and documentation link
checks pass. Python syntax and the actual soak runner pass; Ruff was unavailable.

Logs: `/tmp/soju-phase8-soaks/`, `/tmp/soju-phase8-fuzz-smoke.log` and
`/tmp/soju-step-phase8-final-check-*.log`. The initial harness assertion is in
`/tmp/soju-phase8-fuzz-initial-failure.log`. These temporary paths are convenience
evidence; the workload, outcomes, pins and limits are retained here.
