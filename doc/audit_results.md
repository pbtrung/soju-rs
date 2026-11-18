# Audit and hardening summary

Original audit: 2026-11-17. Latest implementation results: 2026-11-18.

## Why

The audit examined correctness, dependency maintenance, unbounded retained state,
async ownership and persistence, protocol/authentication trust, and maintainability
in this Rust port of soju. The goal is to preserve IRC/database behavior while
making resource limits, lifecycle completion and failure handling explicit.

## What changed

Phases 1–8 and the Jiff migration (STEP-030) are implemented and verified:

- Compiler/Clippy policy, dependency maintenance and release preflight were tightened.
- History/session/notification admission and persistent receipt caches are bounded.
- Writer cancellation, mention draining, autonomous command deadlines and blocking
  actor execution have explicit ownership and regression coverage.
- OAuth discovery binds metadata to its issuer and bounds transport/response handling.
- History, protocol handlers, service callbacks and configuration parsing have
  cohesive responsibilities and validated state contracts.
- Parser properties, bounded mutation corpora, migration/rollback/concurrent-open
  tests and timed resource soaks were added. The new tests found and fixed a
  concurrent SQLite WAL-initialization race.

Detailed changes are in [findings](audit/findings.md), the
[implementation checklist](audit/implementation-plan.md) and
[dated progress](audit/progress.md).

## How it was verified

The final Phase 8 run passes **298 tests per GNU/musl target**, plus reader smoke
and doc tests, strict Clippy on both targets and warning-clean rustdoc. Fixed-seed
mutation campaigns complete 80,000 mutations. Three-minute CAP/writer/history and
slow-push workload models show bounded retained counters and cleanup after load.
See [verification evidence](audit/verification.md) and
[stress workloads/results](audit/resource-soaks.md) for commands, measurements,
resolved failures and the limits of these finite checks.

The Go-soju comparison **did not pass**: twelve scenarios retain capability,
output and timing differences. Original/final Rust comparisons match the same
scenarios after correcting two timing-sensitive fixtures. This supports measured
Rust behavior preservation and does not establish Go equivalence.

## Remaining scope

- Phase 10, STEP-028–029: final deployment/compatibility checks and performance assessment.
- CI automation findings remain open outside the user's implementation scope.
  The removed CI phase has no active checklist; STEP-026–027 remain retired.
- Coverage-guided continuous fuzzing, browser/deployment verification and multi-hour
  soaks are not established by these checks. Memory-only receipt retention and
  actor handoff overhead retain their documented limits.

Use the [record index](audit/README.md) for the source prompt, original baseline,
assessments and supporting guides. The [checklist](audit/implementation-plan.md)
is authoritative for step status; this file stays a concise entry point.
