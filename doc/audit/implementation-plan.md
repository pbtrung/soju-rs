# Audit implementation checklist

[Audit summary](../audit_results.md) · [Record index](README.md)

## 11. Step-by-Step Implementation Plan

This is the authoritative checklist for the 2026-11-17–18 audit. All nine phases
and 28 steps are verified with the limitations in
[final verification](final-verification.md). Finding status remains in
[findings](findings.md); CI findings are open outside implementation scope.

Priorities: P0 is immediate operational protection; P1 is correctness, resource
control or required gates; P2 is maintainability or extended verification.
A completed checkbox records executed work and verification, not a guarantee
about untested deployments. Step descriptions preserve their implementation
instructions and acceptance criteria for review.

The historical execution sequence is recorded in [progress](progress.md).
Native instrumentation was excluded; STEP-026 retains resource measurements.
Phases 7–8 used compilation checks between steps and combined gates after each
phase. This checklist does not authorize remote mutations or release publication.

### Phase 1 — Baseline and Critical Fixes

- [x] **STEP-001 (P0): Establish the audit baseline and live inventory.**
  - Findings: all. Objective/rationale: distinguish pre-existing failures from later regressions.
  - Files: inspect entire repository; create only `doc/audit_results.md` in this execution.
  - Instructions: record commit/toolchain, manifest/lock/features, test/lint/docs/build outcomes, release ordering and dependency sources; preserve existing changes.
  - Dependencies: none. Risks: socket/network sandbox limits and concurrent build timing.
  - Verification: [§12](verification.md#12-verification-matrix) commands; `git diff --exit-code -- src Cargo.toml Cargo.lock .github scripts tests`.
  - Acceptance/status: **Completed (audit only)**; findings/plan delivered, implementation unchanged.

- [x] **STEP-002 (P0): Repair release dispatch and destructive-action preflight.**
  - Findings: REL-001, CI-001. Objective: make release helper behavior match actual workflow files before allowing replacement actions.
  - Files: `scripts/release-new.sh`, `scripts/release-replace.sh`, README; new hermetic release-script tests under `tests/` or `scripts/tests/`.
  - Instructions: dispatch the final existing release workflow; validate it/its inputs before mutation; keep lease/ref/version checks; stub git/gh and assert exact ordering. Use the existing `release.yml` workflow.
  - Dependencies: STEP-001. Risks: script automation compatibility; remote actions must not be used as tests.
  - Verification: `bash -n`; hermetic script test command documented in README; missing-workflow/API/dirty-checkout cases.
  - Acceptance/status: no mutation on failed preflight; valid path dispatches a real workflow; **Completed**.

- [x] **STEP-003 (P1): Make session metric accounting cancellation-safe.**
  - Findings: ASYNC-005. Objective: preserve observability under task cancellation/unwind.
  - Files: `src/lib.rs`, `src/upstream/connect.rs`, focused unit tests.
  - Instructions: introduce a small scope guard that increments once and decrements in Drop; align upstream count scope with the old behavior; no unrelated metrics refactor.
  - Dependencies: STEP-001. Risks: accidental double decrement or changed count definition.
  - Verification: targeted normal/cancel/unwind tests; full `cargo test --workspace --all-features --locked`.
  - Acceptance/status: counts return to prior values after all exits; **Completed**.

### Phase 2 — Rust Modernization

- [x] **STEP-004 (P1): Declare and test compiler support.**
  - Findings: RUST-001. Objective: make the already-modern edition/compiler baseline explicit.
  - Files: `Cargo.toml`, README, toolchain configuration only if it suits distro and CI support.
  - Instructions: use Rust 1.100 as the intended support floor unless a lower floor is explicitly chosen/tested; retain Edition 2024; document compiler/native toolchain setup consistently. Review stable release notes before API changes.
  - Dependencies: STEP-001. Risks: raising the support floor excludes old compilers; document that choice.
  - Verification: fmt/check/test on declared floor, GNU/musl.
  - Acceptance/status: manifest/docs/CI agree; no nightly production feature; **Completed**.

### Phase 3 — Dependency Upgrades

- [x] **STEP-005 (P1): Refresh compatible dependencies against a current index.**
  - Findings: DEPS-001, CI-002. Objective: incorporate stable compatible fixes with a reviewable lock change.
  - Files: `Cargo.toml` rand requirement if desired, `Cargo.lock`, this report.
  - Instructions: rerun live inventory; use refreshed `cargo update --dry-run`, then update compatible releases; inspect every resolved change and target-specific graph. Do not treat the offline zero-update result as current.
  - Dependencies: STEP-004. Risks: transitive behavior/native changes despite semver compatibility.
  - Verification: `cargo tree --duplicates`, `cargo tree -e features`, check/Clippy/test for GNU/musl; audit/deny once tools available.
  - Acceptance/status: rand >=0.10.4 and compatible stable updates resolved or individually justified; lock stable under `--locked`; **Completed**.

- [x] **STEP-006 (P1): Migrate rusqlite across the 0.43 boundary.**
  - Findings: DEPS-001. Objective: incorporate upstream fixes while preserving SQLite behavior/schema.
  - Files: `Cargo.toml`, `Cargo.lock`, `src/database/sqlite/` only for required API adaptations.
  - Instructions: review linked release/compare and new bundled SQLite/native features; check empty/comment-only query behavior, transactions/caches and FTS; keep schema 35 and database format. Record whether native SQLite changes. If any incompatibility cannot be verified, preserve the last tested state and explain.
  - Dependencies: STEP-005. Risks: native build, FTS planner/result ordering, migration errors.
  - Verification: all database tests, old/current schema file reopens, integrity/foreign-key/FTS checks; GNU/musl tests/releases/static ELF checks.
  - Acceptance/status: upgrade passes storage/protocol suite without a schema bump or documented blocker; **Completed**.

- [x] **STEP-007 (P2): Resolve constrained transitives and unnecessary features.**
  - Findings: DEPS-001, CI-002. Objective: maintain the dependency graph without force-breaking parent crates.
  - Files: manifests/lock, dependency usage sites only where parent upgrades require it; dependency policy file if introduced.
  - Instructions: classify [§5](dependency-assessment.md#5-dependency-upgrade-assessment) differences by selected target/parent constraint; read release notes for each breaking parent migration; evaluate Tokio full and optional HTTP/platform features; run machete or compiler-supported unused-dependency diagnostics. Remove only proven-unused requirements.
  - Dependencies: STEP-006. Risks: feature unification differences across tests/targets.
  - Verification: `cargo tree --target all -e features`, cargo audit/deny/machete, all-target tests and cross-target builds.
  - Acceptance/status: each remaining older line has a parent/compatibility reason; no unsupported patch overrides or untested removals; **Completed**.

- [x] **STEP-008 (P1 follow-up): Replace chrono with jiff.**
  - Findings: DEPS-002. Objective: remove the unmaintained timestamp dependency and make the advisory gate pass without suppressions.
  - Files: `Cargo.toml`, `Cargo.lock`, `src/xirc/mod.rs`, timestamp-bearing database/message-store/user/upstream/downstream modules, `src/logger.rs`, `src/server/configuration.rs`, and affected unit/integration fixtures.
  - Instructions: first capture chrono's accepted timestamp/date-range/leap-second behavior and local-time formatting in compatibility fixtures; choose explicit jiff timestamp/duration/time-zone APIs and review the current stable jiff release. Migrate types and conversions in cohesive groups, preserving canonical persisted TEXT values, chronological comparisons, nanosecond parsing and millisecond output. Preserve message IDs and schema 35; no schema bump is justified solely by a Rust time-library change. Remove chrono and its otherwise-unused transitives only after equivalence tests pass.
  - Dependencies: STEP-006–007. Initially added as planning only; the user subsequently authorized implementing the migration and the established commit/push workflow.
  - Risks: changed public Rust timestamp types, date ranges/leap-second acceptance, local timezone and DST behavior, duration overflow, history/read-marker boundaries, expiry and persisted timestamp sorting.
  - Verification: server-time tests for absent and 1–9 fractional digits, invalid dates/bounds and timezone policy; future-clock/read-marker/history tests; configured local-time logging tests; all-target check/Clippy and full tests/doc tests on GNU/musl; migration/reopen/foreign-key/FTS/cursor tests; optimized release/static-link/CLI checks; fresh cargo audit/deny/machete and `cargo tree --invert chrono` confirming removal.
  - Acceptance/status: chrono absent from the resolved graph, canonical database/wire timestamp fixtures unchanged except explicitly reviewed differences, required tests/releases pass, and no chrono advisory suppression added; **Completed**.

### Phase 4 — Compiler and Clippy

- [x] **STEP-009 (P1): Add compiler and core correctness lint policy.**
  - Findings: LINT-001. Objective: enforce high-value checks without an unreviewable all-at-once rewrite.
  - Files: `Cargo.toml`, affected production modules and focused test scopes.
  - Instructions: add prompt rust policy and Clippy all plus panic/todo/unimplemented/dbg/must-use/future checks; handle each diagnostic and ignored send/error by contract. Enable only a set whose violations are resolved in this step.
  - Dependencies: STEP-006. Risks: changing intentional shutdown error handling.
  - Verification: fmt/check/Clippy/tests; inspect lint exception reasons. Completed with 223 tests per GNU/musl target and strict all-target Clippy on both; reader smoke/doc tests pass. External Go/gamja compatibility remains not run; in-process protocol coverage passes. Error-mapping lint review is deferred with the remaining policy rather than changing credential-safe protocol errors in this step.
  - Acceptance/status: enabled policy passes on all targets and does not suppress production errors; **Completed**.

- [x] **STEP-010 (P1): Replace numeric/indexing hazards at external boundaries.**
  - Findings: NUM-001, LINT-001. Objective: make decoding/query/resource arithmetic explicit.
  - Files: `src/msgstore/{bare,db,memory}.rs`, `src/database/sqlite/messages.rs`, `src/rate.rs`, parser/PROXY sites flagged by Clippy.
  - Instructions: checked ID/length/limit conversion; document unlimited-limit policy; preserve intentional ZigZag bit patterns; checked sequence/rate configuration. Use slice patterns/get/split methods instead of repeated indexing after manual checks when clearer.
  - Dependencies: STEP-009. Risks: rejecting values previously accepted by library callers; preserve on-wire valid IDs.
  - Verification: boundary regressions, Go-compatible ID vectors, parser/database tests, selected cast/index lints.
  - Acceptance/status: no unchecked untrusted numeric conversion; each intentional cast/invariant justified; **Completed**.

- [x] **STEP-011 (P1): Propagate setup failures and model validated state.**
  - Findings: PANIC-001, LINT-001. Objective: remove meaningful panic paths, not substitute defaults.
  - Files: `src/auth.rs`, `webpush.rs`, `bin/soju.rs`, `server/configuration.rs`, downstream registration and upstream command/state APIs.
  - Instructions: Result-returning HTTP/TLS/signal/runtime construction; typed pending-command kinds; consolidate borrowed validated registration/upstream state; choose per-lock poison policy. Use narrow documented expectations for unreachable constant-size crypto failures only after review.
  - Dependencies: STEP-010. Risks: internal API propagation and state-machine behavior.
  - Verification: failure injection and invalid transition tests; SASL/cert/auth integration suite; unwrap/expect/panic lints.
  - Acceptance/status: fallible setup errors reach application boundary; no blanket exceptions or fabricated defaults; **Completed**.

- [x] **STEP-012 (P2): Complete pedantic policy and repair documentation.**
  - Findings: LINT-001, DOC-001, ARCH-001. Objective: enforce the final requested practical baseline.
  - Files: manifest lint tables, documented public APIs/tests, all rustdoc warning locations in DOC-001.
  - Instructions: work by diagnostic category; add error/panic contracts, simplify clones/combinators, repair Markdown. Finish long-function items after STEP-022–024 if necessary; do not mark the phase complete until final pedantic check passes. Permit test fixture expectations only with reasons.
  - Dependencies: STEP-011. Ordered dispatcher exceptions are now reviewed locally; larger ARCH-001 decomposition remains STEP-022–024 rather than being claimed fixed by this lint-policy step. Risks: cosmetic churn and overly broad exceptions.
  - Verification: complete baseline from prompt, strict rustdoc and full tests.
  - Acceptance/status: zero unapproved diagnostics, no blanket suppression; **Completed**.

### Phase 5 — Memory and Resource Safety

- [x] **STEP-013 (P1): Define and enforce aggregate memory-history budgets.**
  - Findings: MEM-001, NUM-001. Objective: bound retained history under target/message churn.
  - Files: `src/msgstore/memory.rs`, `src/msgstore/mod.rs`, config/docs if limits are configurable, store unit tests.
  - Instructions: specify target/byte accounting and cursor behavior before implementation; evict coherently; handle recreation without falsely treating old cursors as current; preserve network deletion.
  - Dependencies: STEP-010. Risks: intentional history truncation behavior; document it.
  - Verification: high-cardinality/large-message/cursor/eviction tests; byte/entry counters and sustained RSS experiment.
  - Acceptance/status: bounded counters independent of sender count; retained-history semantics explicitly tested; **Completed**.

- [x] **STEP-014 (P1): Bound accepted sessions including upgrades.**
  - Findings: MEM-002. Objective: cap sockets/tasks/handshake work globally.
  - Files: `src/listen.rs`, `src/listen/http.rs`, server/configuration/config docs, `tests/listeners.rs`.
  - Instructions: acquire admission without spawning unlimited waiters; transfer owned permit through TLS/PROXY/HTTP/WebSocket lifecycle; define operational default and overload response. Do not confuse forwarded identity with transport peer for limits.
  - Dependencies: STEP-009. Risks: connection rejection under legitimate load, upgrade lifetime errors.
  - Verification: incomplete-handshake/upgrade/error/close bursts; task/FD caps and release on all paths.
  - Acceptance/status: no permit leak or upgrade bypass; **Completed**.

- [x] **STEP-015 (P2): Assess receipt-cache growth and enumeration cost.**
  - Findings: PERF-001, MEM-001. Objective: avoid claiming a whole-user budget while cursors grow unchecked.
  - Files: `src/user/delivered.rs`, `src/user/events.rs`, receipt persistence tests/benchmark.
  - Instructions: benchmark cardinality; replace quadratic dedup if beneficial; specify bounded/lazy receipt cache without losing durable cursors or changing playback on reconnect. Keep disk cleanup as a separate documented retention decision.
  - Dependencies: STEP-013. Risks: replay loss/duplicates; ordering.
  - Verification: many-client benchmark, disconnect/restart receipt tests and bounded-cache reload tests.
  - Acceptance/status: measured behavior and explicit retention policy; no unverified eviction; **Completed**.

### Phase 6 — Async and Concurrency

- [x] **STEP-016 (P1): Repair connection writer ownership.**
  - Findings: ASYNC-004. Objective: free the writer/stream when its session is abandoned.
  - Files: `src/conn.rs`, reader/transport if needed, `src/conn/tests.rs`, downstream serve ownership.
  - Instructions: remove writer self-retention or add explicit owned session cancellation; preserve cloned handles/views and graceful queued writes. Clarify what dropping a reader means versus dropping the last session owner; do not use Arc strong-count races as a shutdown protocol.
  - Dependencies: STEP-011. Risks: premature close across clones/views or lost buffered output.
  - Verification: temporary probe converted to regression; idle/blocked/draining writers, serving-task abort, view tests.
  - Acceptance/status: abandoned sessions close and tasks finish within stated bounds; **Completed**.

- [x] **STEP-017 (P1): Add a bounded notification worker.**
  - Findings: ASYNC-001. Objective: constrain notification work before creating tasks.
  - Files: `src/upstream/messages.rs`, `src/user/webpush.rs`, `src/webpush.rs`, user/server lifecycle and metrics.
  - Instructions: bounded queue/worker concurrency; per-target coalescing and explicit overflow result/counter; integrate delayed read-marker checks; handle expiry deletion/cache invalidation. Treat notifications separately from mandatory IRC delivery.
  - Dependencies: STEP-016. Risks: altered push timing/order, superseded read markers, stale work after deletion.
  - Verification: mock slow endpoint flood, repeated MARKREAD, shutdown/deletion, subscription-expiry and registration-limit tests.
  - Acceptance/status: queue/task/request limits hold; no unbounded waiter tasks; **Completed**.

- [x] **STEP-018 (P1): Drain durable mention work before user completion.**
  - Findings: ASYNC-003. Objective: preserve mention state on orderly stop and retry transient failures.
  - Files: `src/upstream/messages.rs`, `src/user/events.rs`, `src/user/network.rs`, DB helpers and persistence tests.
  - Instructions: own flush scheduling, retain failed batches and merge timestamps monotonically; flush/join before completion; cancel work safely on network/user deletion; keep DB calls off executor threads as settled in STEP-020.
  - Dependencies: STEP-016; responsiveness verification after STEP-020. Risks: shutdown hangs, resurrection after deletion.
  - Verification: stop-before-delay/reopen regression, injected write failure/recovery, delete-during-flush.
  - Acceptance/status: successful orderly shutdown includes the latest durable mention; errors reported with defined policy; **Completed**.

- [x] **STEP-019 (P1): Implement autonomous pending-command timeout.**
  - Findings: PROTO-001, PANIC-001. Objective: avoid permanently stuck serial queues and lost terminal replies.
  - Files: `src/upstream/commands.rs`, connection/timer events, `src/user/events.rs`, upstream server tests.
  - Instructions: typed active deadline event keyed by connection/command generation; complete downstream failures, advance or reconnect per late-response policy; stop timers on disconnect/deletion; preserve early-error correlation.
  - Dependencies: STEP-011, STEP-016. Risks: late replies routed to next request.
  - Verification: paused-time no-reply/no-third-request regression; late reply, label, disconnect and queue-cap tests.
  - Acceptance/status: requests terminate within policy without misrouting; **Completed**.

- [x] **STEP-020 (P1): Resolve current-thread database stalls.**
  - Findings: ASYNC-002. Objective: keep networking/timers responsive under persistence contention.
  - Files: `src/server.rs`, `src/bin/soju.rs`, `src/user/events.rs`, DB execution boundary and contention integration tests.
  - Instructions: measure the existing stall, then choose the smallest ownership-safe runtime/blocking-worker design; retain event serialization. Ensure started blocking work and shutdown have defined completion semantics.
  - Dependencies: STEP-009. Risks: worker handoff overhead and actor ordering.
  - Verification: current-thread/multi-thread contention, expiry/VACUUM, latency and normal-throughput comparison.
  - Acceptance/status: stated timer/connection latency bound met under controlled contention; no per-message unbounded spawning; **Completed**.

- [x] **STEP-021 (P1): Validate OAuth discovery trust and body limits.**
  - Findings: AUTH-001. Objective: bind provider metadata to the configured issuer.
  - Files: `src/auth.rs`, config/auth docs, mock HTTP auth tests.
  - Instructions: RFC issuer equality validation, secure endpoint/redirect policy and byte-bounded response decoding; retain intentional local testing support only through explicit policy. Preserve token/secret redaction.
  - Dependencies: STEP-011. Risks: provider configurations previously accepted become rejected; document required changes.
  - Verification: matching/mismatch discovery, HTTP/HTTPS/redirect/oversized/error/token cases, external auth integration tests.
  - Acceptance/status: mismatched issuer cannot authenticate, configured compatibility documented, secrets absent from logs; **Completed**.

### Phase 7 — Architecture Refactoring

- [x] **STEP-022 (P2): Separate history request parsing, query and response.**
  - Findings: ARCH-001, LINT-001. Objective: reduce the largest downstream validation/DB/reply function.
  - Files: `src/downstream/cmds/history.rs`, new cohesive history submodules only if helpful, history/read-marker tests.
  - Instructions: represent subcommands/bounds/limits in a typed request; parse once, execute store operation, format reply; preserve future-clock/tie/order/limit semantics and network scope. Keep READ changes separate if they do not share logic.
  - Dependencies: STEP-010, STEP-018. Risks: protocol boundary inclusivity/order changes.
  - Verification: BEFORE/AFTER/AROUND/BETWEEN/TARGETS, empty histories, equal timestamps, future clocks and both-runtime tests; compatibility transcript.
  - Acceptance/status: clean responsibilities, no changed valid transcript, no duplicate validation; **Completed**.

- [x] **STEP-023 (P2): Simplify protocol state handlers.**
  - Findings: ARCH-001, PANIC-001. Objective: separate command responsibilities and validated state access.
  - Files: `src/upstream/{users,channels,registration,messages,dispatch}.rs`, `src/downstream/{registration,welcome,caps}.rs`, `src/user/events.rs` as justified.
  - Instructions: one command-family extraction at a time with existing fixture/transcript preserved; borrow validated connection state rather than repeated macro unwraps; avoid new dynamic-dispatch layers. Do not fragment small cohesive helpers just to satisfy line counts.
  - Dependencies: STEP-016–020. Risks: event ordering, stale-connection handling, labeled-response/batch context.
  - Verification: targeted protocol tests after each extraction, full dual-runtime suite and pinned compatibility harness.
  - Acceptance/status: complex functions are cohesive and lint-reviewed; unchanged protocol/state behavior; **Completed**.

- [x] **STEP-024 (P2): Separate service registry and configuration concerns.**
  - Findings: ARCH-001, LINT-001. Objective: make large declarative registries and directive parsing easier to review.
  - Files: `src/service/mod.rs`, `src/config.rs`, command registry/parser modules as needed.
  - Instructions: move command declarations into a registry module; preserve admin/global/user constraints and help output; extract directive validation where coherent; replace `Option<&mut User>` assumptions through a validated dispatch contract where practical.
  - Dependencies: STEP-011, STEP-023. Risks: privilege/flag/default behavior regressions.
  - Verification: admin/non-admin/deferred/self-user/cross-user service scenarios; config roundtrips/errors; help transcript.
  - Acceptance/status: authorization invariant preserved; no arbitrary empty-user defaults; **Completed**.

### Phase 8 — Testing and Fuzzing

- [x] **STEP-025 (P1): Add parser properties, fuzz targets and migration stress cases.**
  - Findings: TEST-001, NUM-001. Objective: cover malformed inputs and persistence invariants beyond fixed fixtures.
  - Files: parser tests, database migration/storage tests, a separate `tests/fuzz/` package and corpus.
  - Instructions: properties for IRC tags/parse-format, BARE/IDs, UTF-8 framing, multiline and durations; fuzz IRC/PROXY/BARE/scfg with explicit size budgets; add rollback/foreign-key/FTS integrity and genuinely concurrent file-backed initialization tests. Validate schemas 30–35 and older supported fixtures without modifying existing historical schemas.
  - Dependencies: relevant fixes and STEP-022–024. Risks: nondeterministic tests and unbounded generators.
  - Verification: deterministic property seeds, finite fuzz smoke runs, minimized crashing corpus, `PRAGMA integrity_check`, `foreign_key_check`, FTS integrity checks.
  - Acceptance/status: realistic adversarial coverage, no removed tests, documented corpus/seed reproduction; **Completed**.

- [x] **STEP-026 (P2): Measure sustained resource stability.**
  - Findings: TEST-001, MEM-001/002, ASYNC-001/004. Objective: measure long-lived resource retention under controlled workloads.
  - Files: resource benchmark harness, local soak scripts and performance documentation.
  - Instructions: run repeated connect/disconnect, many-sender and slow-push soaks with defined workload duration and limits; retain all regression tests and distinguish finite probes from sustained measurements.
  - Dependencies: STEP-013–021, STEP-025. Risks: host load, allocator behavior and workload selection affect measurements.
  - Verification: record workload, duration, task/FD/byte counters and RSS; compare retained state and teardown after load stops.
  - Acceptance/status: resource plateau and cleanup measured rather than inferred; **Completed**.

### Phase 9 — Final Verification

- [x] **STEP-027 (P1): Run complete regression/deployment verification.**
  - Findings: all. Objective: demonstrate the final implementation preserves IRC/database/deployment behavior.
  - Files: full repository, `doc/audit_results.md` verification/progress sections.
  - Instructions: all prompt commands, both-runtime integrations on GNU/musl, full release archive builder, ELF interpreter/NEEDED assertions, checksum/archive CLI smoke tests, pinned IRCv3/plumbing/gamja/differential harness. Validate existing schema 35 database backup/reopen and migration fixtures; do not use a production DB as a test input without a safe copy.
  - Dependencies: in-scope P1 fixes and STEP-025–026; unresolved optional steps must be explicitly carried forward. Risks: external compatibility drift and native/target differences may expose failures missed by the host-only checks.
  - Verification: [§12](verification.md#12-verification-matrix) full matrix rerun after implementation; `scripts/build-release.sh` and archive smoke checks without any remote release action.
  - Acceptance/status: required gates PASS, no unexplained protocol/schema differences, remaining findings assigned honest statuses; **Completed with documented limits**. GNU/musl gates and restored-archive deployment pass; IRCv3 32/32, plumbing 20/20, local Gamja 4/4 on each bouncer and Senpai real-bouncer checks pass. Raw Go transcript differences remain; original/final Rust scenarios pass with service-completion fixture corrections. See [final verification](final-verification.md).

- [x] **STEP-028 (P2): Measure performance and close the audit record.**
  - Findings: PERF-001, ARCH-001, resource/concurrency findings. Objective: substantiate performance preservation and production-readiness limits.
  - Files: benchmark/report documentation, `doc/audit_results.md`.
  - Instructions: build baseline/final optimized reader binaries, alternate same-workload measurements without simultaneous compilation; measure DB replay/target-list latency, single-CPU responsiveness, push saturation and repeated-connection resources. Record settings/hardware/sample ranges; update all findings only after implementation and verification.
  - Dependencies: STEP-027. Risks: noisy shared host and synthetic benchmarks overclaiming real throughput.
  - Verification: reader benchmark, existing connect-pages benchmark where practical, representative file-backed history and soak runs.
  - Acceptance/status: material regressions explained/resolved, no unsupported speedup/memory-safety claims, final limitations explicit; **Completed with documented limits**. Alternating original/final optimized reader samples, one-CPU actor contention/handoff costs, receipt enumeration, file-backed TARGETS/replay/backup integrity, connect-pages and final 180-second resource soaks are recorded in [final verification](final-verification.md). No universal performance or deployment-capacity claim; CI findings remain open.
