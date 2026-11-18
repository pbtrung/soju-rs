# Audit implementation progress

[Audit summary](../audit_results.md) · [Record index](README.md)

## 15. Implementation Progress Log

This timeline summarizes what changed. The
[checklist](implementation-plan.md) owns acceptance/status, and
[verification](verification.md) preserves checks, failures and limits. Step IDs
follow the current sequential checklist, including in historical entries.

### 2026-11-17: baseline and implementation through Phase 6

The original audit inspected source, storage, scripts and workflows, ran GNU/musl
baseline checks, researched dependencies and reproduced metric cancellation,
writer ownership and history-retention problems. STEP-001 completed the
assessment without implementation. Baseline passes did not establish that the
identified problems were fixed.

| Step | Implemented outcome | Verification at completion |
| --- | --- | --- |
| STEP-002 | Release dispatch/preflight before remote mutations | Six hermetic tests and shell syntax; CI-001 remains open |
| STEP-003 | RAII session metric accounting | Cancellation/unwind regressions; 213 tests |
| STEP-004 | Declared Rust 1.100 floor, Edition 2024 retained | 213 tests per GNU/musl target |
| STEP-005 | Compatible dependency refresh, including rand 0.10.4 | 213 tests per target; releases; chrono maintenance warning retained |
| STEP-006 | rusqlite 0.43 with SQLite/schema/IDs preserved | 214 tests per target; migration/reopen/FTS/FK; deny still fails on chrono |
| STEP-007 | Parent-version assessment and unused feature reduction | 214 tests per target; machete/license/source pass; chrono still unresolved |
| STEP-008 | Jiff migration with explicit timestamp compatibility | 219 tests per target; time/storage/DST fixtures; audit/deny/machete pass |
| STEP-009 | Core lint policy and operational-error handling | 223 tests per target; strict Clippy; rustdoc warnings remain |
| STEP-010 | Checked numeric boundaries and ID/rate overflow | 227 tests per target; Go ID vectors retained |
| STEP-011 | Fallible setup and validated state/poison policy | 233 tests per target; setup/state/poison regressions |
| STEP-012 | Complete practical lint baseline and rustdoc repair | 233 tests per target; strict Clippy/rustdoc pass without broad suppressions |
| STEP-013 | Aggregate memory-history budgets and cursor incarnations | 235 tests per target; finite churn/RSS measurement |
| STEP-014 | Shared physical-session admission, default 256 | 240 tests per target; handshake/upgrade/reload/abort regressions |
| STEP-015 | Bounded persistent receipt cache and lazy reconnect pages | 246 tests per target; cache failure/reload/alias/replay checks |
| STEP-016 | Writer/session ownership, cancellation and graceful drain | 254 tests per target; clone/view/blocked/abort coverage |
| STEP-017 | Bounded notification worker and scoped subscription cleanup | 264 tests per target; flood/timing/expiry/owner/admission coverage |
| STEP-018 | Actor-owned mention batches, retries and shutdown drain | 269 tests per target; stop/reopen/rollback/delete/permanent-error coverage |
| STEP-019 | Autonomous command deadlines and reconnect isolation | Combined 282-test GNU/musl suites; late/label/epoch/queue-cap coverage |
| STEP-020 | Bounded blocking actor jobs and tracked maintenance | Combined gates; contention responsiveness and explicit handoff cost |
| STEP-021 | Issuer-bound OAuth discovery, HTTPS policy and body limits | Combined gates; mismatch/redirect/limit/redaction coverage |

The scope excluded native instrumentation while retaining finite resource
measurements and unresolved CI findings. STEP-019–021 implementations preceded
combined final Phase 6 tests/lint/gates. GNU/musl compiler, tests, release and
dependency results are in [Phase 6 verification](verification.md#final-verification-through-phase-6).

### 2026-11-18: architecture, stress coverage and final verification

| Step | Implemented or measured outcome | Verification |
| --- | --- | --- |
| STEP-022 | Typed history parsing/query/reply boundaries | Tie/shortcut/future-clock assertions and original/final Rust comparisons |
| STEP-023 | Cohesive upstream/downstream handlers with generation/label/batch ownership | Combined dual-runtime suite; protocol comparisons with documented fixture barriers |
| STEP-024 | Typed service registry and ordered configuration validation | Help/scope/config snapshots, including assertions against original Rust |
| STEP-025 | Parser properties, bounded mutation corpora and SQLite integrity/races | 298 tests per target; 80,000 mutations; WAL retry fix and repeated concurrent opens |
| STEP-026 | Timed connection/history and slow-push workload harnesses | 180-second workloads plus five-second drain; retained counters/cleanup measured |
| STEP-027 | Final gates, archives/restored schema-35 deployment, IRCv3, transports and local client forks | 298 tests per target; IRCv3 32/32, plumbing 20/20, Gamja 4/4 per bouncer, Senpai real-bouncer pass |
| STEP-028 | Alternating readers, one-CPU actor tradeoff, receipts, SQL and final resource soaks | Ranges and limitations retained in the final assessment |

Phase 7 is commit `bd80539`; its combined GNU/musl suites pass 287 tests per
target. Phase 8 parser/database coverage and WAL fix are in `d62877e`, and
resource harnesses in `a953ac3`. Phase 9 is `91654b9`; it persists service-reply
completion barriers after the Rust comparison exposed dependent-command races
in the fixture.

The [final assessment](final-verification.md) records settings, pins and results.
All twelve raw Go comparisons retain differences; the original/final Rust
selection passes with the service-fixture correction. Dependency scanners were
absent in the final session, so earlier passing scans are not fresh advisory
evidence. CI findings, actor overhead, memory-only receipt retention and finite
workload/deployment limits remain explicit.


### Documentation maintenance after verification

The documentation was reorganized into operator, client, development and release
guides. The audit timeline was condensed with detailed evidence retained in the
findings and verification records. Session-local artifact references were removed;
configuration examples, dependency-review dates and schema/resource descriptions
were checked against the implementation. This documentation review adds no new
runtime verification claim or implementation phase.
