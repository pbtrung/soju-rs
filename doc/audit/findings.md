# Audit findings and verified status

[Audit summary](../audit_results.md) · [Record index](README.md)

## 3. Audit Findings

Status denotes **implementation status** through Phase 8. The original audit
was assessment-only; completion notes record each step's verification at its
completion date, with the latest combined checks in [verification](verification.md).
Problem statements and source locations describe the discovery snapshot unless
noted otherwise. Severity reflects practical impact; an unbounded structure alone
does not establish a remotely demonstrated denial of service.

### 3.1 Critical Issues

None confirmed. Fuzzing and long-running load testing remain outstanding; absence of a critical finding is not a security certification.

### 3.2 High-Priority Issues

#### REL-001 — Release helpers dispatch a nonexistent workflow

- **Severity / classification:** High; confirmed operational defect by repository inspection.
- **Location:** `scripts/release-new.sh:92`, `scripts/release-replace.sh:52–58`; existing workflows are `.github/workflows/tests.yml` and `release.yml`.
- **Problem:** Both helpers dispatch `ci.yml`, which is absent. The replacement helper first pushes the branch, moves the remote tag and can delete the existing release/assets. Dispatch then fails against this repository configuration. README also describes a combined CI/release workflow that no longer exists.
- **Impact:** New release automation fails; replacement can remove the current release without starting its replacement. No remote mutation was performed in this audit.
- **Recommended fix:** Establish the final tested release workflow, update both dispatches and documentation, and preflight workflow availability before any destructive operation. Add local stub-based script tests that record command ordering and simulate missing workflows/API failures. Preserve leases and clean-checkout guards.
- **Verification:** `bash -n scripts/release-*.sh`; hermetic shell test with stub `git`/`gh`; ensure failed preflight performs no push, tag move or delete; verify dispatched filename exists and accepts `publish`.
- **Status:** Completed. Release helpers now preflight the active remote release.yml against the local definition before mutation; six hermetic regression tests and bash syntax checks pass. Ruff and ShellCheck are unavailable. CI-001 remains open and outside the requested implementation scope.

#### CI-001 — Tests are manual and publishing does not require them

- **Severity / classification:** High; confirmed enforcement gap.
- **Location:** `.github/workflows/tests.yml:4–5,56–60`; `.github/workflows/release.yml:4–5,18–103`.
- **Problem:** No push or pull-request trigger. The publish job needs only the archive build. That build runs neither the Rust tests nor the compatibility tests. No workflow enforces formatting, Clippy, rustdoc warnings or the full requested quality gates.
- **Impact:** Regressions can merge or ship despite failing tests. README's claim that tests pass before publishing is unsupported by the workflow graph.
- **Recommended fix:** Add automatic checks and make the release build/publish graph depend on checks of the same commit. Use a reusable test workflow or equivalent dependency; retain explicit publishing authorization and minimal permissions.
- **Verification:** Inspect job dependencies and event triggers; use a local workflow validator, then in a separately authorized CI run demonstrate that a failing check prevents artifact publication.
- **Status:** Not Started; CI-hardening implementation is excluded by user direction. This finding remains open.

#### MEM-001 — In-memory history has no aggregate target or byte limit

- **Severity / classification:** High; confirmed unbounded retention; production exhaustion not load-tested.
- **Location:** `src/msgstore/memory.rs:7,39–46,78,83–84,106–118`; `src/upstream/messages.rs` incoming private-message buffer selection.
- **Problem:** The 4,096-message cap is per target. `buffers.entry(...)` creates another ring for every distinct target; only network deletion/store close remove them. Message size is also not accounted in the history budget. Repeated distinct private-message senders can grow memory throughout a network's lifetime.
- **Impact:** Heap growth despite per-connection bounds. In a temporary public-API probe, the first target remained after appending messages to 10,000 different targets.
- **Recommended fix:** Define a per-user/network byte and target budget with explicit eviction/history-expiry semantics. Account retained messages and target keys, release empty entries, and preserve monotonically meaningful delivery cursors when a target is evicted/recreated. Also evaluate receipt/client-name retention before claiming a whole-user bound.
- **Verification:** Regression with many unique targets, large/multiline messages and repeated eviction; assert byte/entry ceilings, defined replay behavior and network cleanup. Measure memory under sustained sender churn.
- **Status:** Completed. Bound persistent receipt dirty caches per user to 4,096 entries/4 MiB with write-through pressure handling, lazy 128-row reconnect pages, upsert-only partial flushes and failure-safe accounting. Preserve memory-only receipt lifetime and explicitly exclude it from whole-user memory claims. Seven-sample many-client benchmark documents small-client overhead and removes quadratic enumeration (4,096 clients: 393.526 ms to 3.052 ms). Shared pressure, 10k-row file reload, aliases/legacy bytes, failure/atomic initialization and reconnect/restart tests pass. Full GNU/musl checks (246 tests each), strict Clippy/rustdoc, cargo audit/deny/machete pass; schema 35 and durable rows preserved.

#### ASYNC-001 — Ordinary Web Push work bypasses admission limits

- **Severity / classification:** High; confirmed unlimited task admission; maximum observed production concurrency unmeasured.
- **Location:** `src/upstream/messages.rs:138–169`; `src/user/webpush.rs:43–54,97–149`; contrast bounded registration checks in `src/downstream/cmds/webpush.rs:193–244`.
- **Problem:** Each qualifying message spawns a task holding cloned message/server/network state, sleeps five seconds, queries SQLite and sends notifications. MARKREAD broadcasts also spawn tasks. The registration-test semaphore does not cover these paths. A 15-second HTTP timeout limits individual operations, not task creation or queued work across subscriptions.
- **Impact:** A slow push endpoint plus sustained mentions/private messages can accumulate tasks, payloads, DB jobs and requests. These tasks are not joined on shutdown.
- **Recommended fix:** Admit work before spawning through a bounded notification worker/queue. Define per-target coalescing, concurrency and overflow policy; expose overload counters; cancel or drain deliberately on network/user/server shutdown. Do not silently discard IRC messages when notification capacity is exhausted.
- **Verification:** Slow/mock sender tests on both runtimes; burst beyond queue capacity, assert bounded tasks/bytes, documented coalescing, cancellation, and continued IRC responsiveness.
- **Status:** Completed. One bounded/coalescing worker per user admits at most 128 pending conversations/1 MiB and one active job; sender-wide semaphore limits encryption/HTTP to 32 requests. Integrate delayed read checks, MARKREAD, overflow metrics, joined stop, network cancellation and registration-test stop tokens. Recheck scoped subscription snapshots and conditionally delete expiry to protect replacement IDs/keys; invalidate subscription caches. Flood/timing/byte/cancel/expiry/owner/global-cap/25-subscription/four-test-slot regressions pass. Full GNU/musl checks (264 tests each), strict Clippy/rustdoc and documentation checks pass; stale Unix-socket fixture now waits for actual refusal.

### 3.3 Medium-Priority Issues

#### MEM-002 — Accepted connection tasks have no global admission bound

- **Severity / classification:** Medium; confirmed absence of application admission limits, potential exhaustion risk.
- **Location:** `src/listen.rs:184–215,228–318`; `src/listen/http.rs:77–113,265–283`.
- **Problem:** Every accepted socket gets a task; TLS/PROXY/registration timeouts and bounded per-connection queues do not cap simultaneous connections. HTTP sessions and idle admin connections also retain resources.
- **Impact:** Connection bursts multiply bounded per-connection allocations and consume file descriptors/handshake work. Deployment proxy/OS limits may mitigate this but are not enforced here.
- **Recommended fix:** Add configurable listener/global admission permits held for the full accepted connection lifetime, including WebSocket upgrades; avoid spawning tasks merely to wait for a permit. Document resource-limit deployment guidance. Consider per-peer policy only where trusted proxy semantics are correct.
- **Verification:** Saturate the limit with incomplete handshakes/idle sessions; assert active task/FD ceiling, permit release on errors and no upgrade bypass.
- **Status:** Completed. Global accepted-session admission defaults to 256, is reloadable, rejects before spawning, and follows physical streams through TLS/PROXY/HTTP/WebSocket upgrades. Cancellation closes session writers. Concurrent admission, 300 overload attempts, close/error/upgrade/reload/admin/shutdown/abort regressions pass; full GNU/musl checks (240 tests each), strict Clippy/rustdoc and documentation links pass.

#### ASYNC-002 — Single-thread runtime executes blocking DB handlers inline

- **Severity / classification:** Medium; confirmed scheduling behavior, stall magnitude needs measurement.
- **Location:** `src/server.rs:31–43`; `src/user/events.rs:72`; `src/bin/soju.rs:122–132`; `src/database/sqlite/mod.rs:83–98`.
- **Problem:** `server::blocking` calls `f()` directly on a current-thread runtime. User event handlers perform synchronous SQLite calls and mutex acquisition. Production deliberately selects this runtime on one CPU. DB maintenance/VACUUM or an external writer can stall its sole async thread.
- **Impact:** All connections, timers and shutdown handling can pause while SQLite waits; existing dual-runtime tests do not simulate lock contention or a large database.
- **Recommended fix:** First measure timer/connection latency with SQLite contention. Choose a documented runtime/worker strategy that keeps blocking database/event work off the only executor thread while preserving serialized user ownership. A multi-thread runtime with constrained worker count is an option; an owned blocking actor is another. Do not attempt `block_in_place` on current-thread.
- **Verification:** Both-runtime contention tests with a separate DB connection holding a transaction and maintenance active; assert chosen responsiveness bound and ordering. Benchmark normal traffic before/after.
- **Status:** Completed. Owned actor handlers execute behind a 16-job blocking admission limit on both runtimes, preserving event serialization and supervision. Tracked maintenance coalesces expiry/VACUUM requests and joins on stop. Both-runtime 600-ms writer-lock regressions preserve independent I/O/10-ms timer responsiveness within 200 ms, order, 1,000-reload task bounds and shutdown cleanup. Optimized same-harness legacy/current comparison measures a current-thread timer at 329.789 ms versus 10.215 ms; handoff and parsed-IRC throughput costs are recorded in resource-limits.md. Final GNU/musl 282-test suites, compiler/strict Clippy/rustdoc, release/static-link/CLI and dependency gates pass.

#### ASYNC-003 — Delayed mention persistence is not drained at shutdown

- **Severity / classification:** Medium; confirmed lifecycle gap; end-to-end loss regression still required.
- **Location:** `src/upstream/messages.rs:369–409` (`record_mention`); `src/user/events.rs:350–367`; `src/server.rs:238–250`.
- **Problem:** Mentions wait one second in a detached task. Stop closes connections and saves delivery receipts, but does not flush/join `pending_mentions`. Server shutdown waits only for users; runtime teardown may cancel the delayed task before its DB write. Taking the pending map before a failed write also loses its retry data.
- **Impact:** A recently received message can remain in history without its mention marker after orderly restart, or after transient persistence failure.
- **Recommended fix:** Make mention flushing user/network-owned, preserve batches until successful persistence, and drain them before signalling user completion. Define retry/backoff and deletion behavior; avoid writing markers for a deleted network.
- **Verification:** Receive a mention then stop before one second; reopen a file DB and assert marker persisted. Inject write failure then recovery, and delete network/user during a pending flush.
- **Status:** Completed. Replace detached mention flushes with actor-owned maps/deadlines and joined blocking batch transactions. Clear only after commit, retain/merge failures and retry autonomously; stop flushes all batches with three attempts and explicit failure metrics/logs. Network updates transfer state, deletion is serialized and foreign keys prevent stale resurrection. Stop-before-delay/reopen, rollback/recovery, monotonic/delete-during-flush and permanent-error tests pass. Full GNU/musl checks (269 tests each), strict Clippy/rustdoc and documentation checks pass. STEP-019 subsequently addressed handler responsiveness.

#### ASYNC-004 — Dropping connection handles does not terminate the writer

- **Severity / classification:** Medium; reproduced resource-lifecycle defect in public connection API; normal explicit-close paths already exist.
- **Location:** `src/conn.rs:79–89,141–159,163–171`; `Conn`/`ConnReader` have no drop cancellation contract.
- **Problem:** The writer owns a `Conn` clone, which owns its channel sender. Dropping all external handles cannot close the channel while that writer waits on its receiver. Without explicit close/shutdown it keeps the writer side of the stream and shared state alive. Panic/cancellation exits can bypass explicit cleanup.
- **Impact:** Orphaned idle writer tasks/sockets. Temporary duplex probe dropped the reader and connection and got a timeout, rather than EOF, when reading the peer. This proves the ownership behavior, not a socket leak on every normal disconnect.
- **Recommended fix:** Give a session owner/drop guard explicit cancellation responsibility, or remove sender retention from writer-owned state so last external sender drop terminates the writer. Preserve view lifetime semantics and explicit graceful drain.
- **Verification:** Drop all handles with idle, buffered and blocked writers; peer must close within a defined bound. Abort serving tasks, check cleanup; preserve view and graceful-shutdown regressions.
- **Status:** Completed. Remove writer-owned senders/session owners. Explicit shared ownership keeps clones/views valid, cancels abandonment and rate/write waits, and preserves requested graceful draining. Attachment guards remove actor clones and persist receipts after serving-task abort. Idle/blocked/rate/drain/view and both-runtime abort regressions pass, including the ten-second abandoned-writer close bound. Full GNU/musl checks (254 tests each), strict Clippy/rustdoc and documentation checks pass.

#### PROTO-001 — Pending-command expiry occurs only on another enqueue

- **Severity / classification:** Medium; confirmed missing autonomous timeout and missing terminal response; no labeled-response attack established.
- **Location:** `src/upstream/commands.rs:80–137` (`enqueue_command`), especially `sent_at` expiry and `l.drain`.
- **Problem:** The 30-second timeout is evaluated only when another command of the same type is submitted. With no later enqueue an unanswered head blocks queued commands indefinitely. On eventual expiry it is logged/dropped without completing the downstream response (the source has a corresponding TODO).
- **Impact:** WHO/WHOIS/LIST/authentication requests can remain pending or disappear without a terminal result. Late upstream replies require careful handling before advancing a serial queue.
- **Recommended fix:** Add an owned deadline event/timer for the active head, complete affected downstream requests, and specify how late replies are isolated. If isolation is impossible without labels, reconnect or use an explicitly documented recovery policy rather than misroute replies.
- **Verification:** Paused-time test: enqueue two requests, upstream never replies, no third enqueue; assert bounded completion. Add late-reply, early-error, disconnect and labeled-response cases.
- **Status:** Completed. Autonomous actor deadlines expire active serial commands after 30 seconds without another request. Typed kinds, connection IDs and owned command epochs reject stale events; timeout aborts every queue with labeled terminal replies and reconnects to isolate late replies. Paused-time no-third-request, late/label, stale-epoch and 64-entry-cap regressions pass. Combined final GNU/musl suites pass 282 tests each with strict Clippy on both targets, strict rustdoc, release/static-link/CLI checks and dependency policy.

#### LINT-001 — Required correctness policy is absent and fails when trialled

- **Severity / classification:** Medium; confirmed policy gap/diagnostics, not thousands of verified bugs.
- **Location at discovery:** `Cargo.toml` (no lint tables), `src/conn/reader.rs:49`, `src/upstream/mod.rs:35–39`, protocol handlers, tests and BARE/message-ID conversions.
- **Problem at discovery:** Default Clippy passes; enabling pedantic plus requested restriction lints fails. The trial reported 1,773 errors for the library and 2,453 for the lib-test target; these overlap and must not be summed as unique defects. Examples include indexing, unwrap/expect, unchecked casts, missing contracts and oversized functions.
- **Impact:** Desired invariants are not mechanically enforced. Many diagnostics represent checked local invariants/test assertions and need a reasoned disposition, not arbitrary fallback values.
- **Recommended fix:** Stage explicit Cargo rust/Clippy policies; use checked conversion/borrowed-state APIs and typed variants; document small unavoidable invariant/test exceptions with precise `expect` reasons. Never enable all restriction lints. Include unused-must-use and unsafe prohibition from the prompt.
- **Verification:** Full all-target Clippy command with the final manifest policy and `-D warnings`; check exception reasons and unfulfilled expectations on Rust 1.100. Re-run behavior tests after each logical group.
- **Status:** Completed. Completed the prompt compiler/Clippy baseline, including pedantic, indexing/string-slice and error-mapping checks. Added API error/panic contracts, repaired rustdoc links/grammar, reused clones and preserved fixed protocol messages while retaining safe error causes. Reviewed statement-level bounds contracts and function-specific ordered-dispatch exceptions; ARCH-001 was deferred at STEP-011 completion and was subsequently addressed by STEP-021–023. No crate/module-wide lint suppression was added. fmt/check and strict GNU/musl all-target Clippy pass; all 233 tests per target plus reader smoke/doc tests pass. Strict rustdoc now passes with zero warnings.

#### PANIC-001 — Fallible setup and state invariants use panicking APIs

- **Severity / classification:** Medium; confirmed panic sites, generally potential failure paths rather than proven remote triggers.
- **Location:** `src/auth.rs:47–58`, `src/webpush.rs:282–307`, `src/bin/soju.rs` runtime/signal setup, `src/server.rs` lock unwraps; `src/upstream/commands.rs:55,91`, `src/downstream/registration.rs` state unwraps, `src/upstream/mod.rs:35–39`.
- **Problem:** HTTP/TLS/runtime initialization failures can panic instead of returning startup errors. Protocol dispatch relies on optional state plus unchecked indexing/unwraps; unsupported pending commands panic. Mutex poison can propagate panics outside the supervised user task.
- **Impact:** Harder operational diagnosis/recovery; invalid internal transitions can terminate a user or process. Guarded fixed-length cryptographic conversions are different and should not be described as reachable parser bugs.
- **Recommended fix:** Propagate setup errors, encode pending command kinds and registration/upstream phases in types where useful, borrow validated state once, and explicitly choose poison recovery/fail-stop policy per lock. Preserve real errors. Keep constant-size crypto invariant exceptions narrow.
- **Verification:** Failure-injection setup tests, malformed/state-transition tests, Clippy, and existing certificate/SASL/authorization integration tests. No blanket panic-catching or defaulting credentials.
- **Status:** Completed. Made HTTP/TLS/Web Push setup fallible, propagated runtime/signal errors, typed pending-command kinds, and checked registration/upstream/service state. Removed operational unwrap/expect paths and documented fail-closed actor/auth locking separately from SQLite rollback-based recovery. Added six setup/state/poison regressions; limited invariant/fixture expectations to reasoned local scopes. Enabled unwrap/expect lints. fmt/check/strict Clippy and 233 GNU/musl tests plus reader smoke/doc tests pass. Rustdoc retains the 13 existing warnings for STEP-011.

#### DEPS-001 — Current patch and semver-breaking dependency updates exist

- **Severity / classification:** Medium; confirmed freshness gap; no reachable dependency exploit asserted.
- **Location:** `Cargo.toml` dependencies `rand = "0.10.3"`, `rusqlite = "0.42.0"`; `Cargo.lock`.
- **Problem:** Registry live results show 0.10.4 and 0.43.0 respectively. Upstream rusqlite release notes include memory-safety fixes in APIs outside the explicitly enabled application feature set; assess feature reachability instead of assigning a vulnerability to every use. Transitive latest-version differences appear in [§5](dependency-assessment.md#5-dependency-upgrade-assessment).
- **Impact:** Missed maintenance fixes; SQLite wrapper updates carry behavioral/native/FTS compatibility risk and need dedicated verification.
- **Recommended fix:** Separate rand/compatible lock refresh from rusqlite 0.43 migration. Read release notes, keep bundled SQLite and review native versions/features. Upgrade parent crates for constrained transitives; do not force incompatible transitive versions with patches.
- **Verification:** Current registry refresh, `cargo tree -e features`, `cargo audit`, `cargo deny`, GNU/musl tests/releases; schema 35 migration/reopen/FTS/ID-exhaustion tests. No schema bump solely for a wrapper upgrade.
- **Status:** Completed. Completed version/feature assessment: all 34 retained older versions have documented parent constraints. Removed unused Tokio process/stdio and SQLite WASM features while preserving native cache/locking; removed two lockfile packages. fmt/check/Clippy, 214 GNU/musl tests, reader smoke/doc tests and releases/static-link/CLI pass. Machete and license/source checks passed; at STEP-007 completion full deny failed on DEPS-002. The later STEP-030 migration resolves that advisory.

#### DEPS-002 — Replace unmaintained chrono with jiff

- **Severity / classification:** Medium; confirmed maintenance advisory, not a demonstrated exploitable vulnerability.
- **Location at discovery:** `Cargo.toml` chrono dependency; timestamp types in `src/database/mod.rs`, `src/msgstore/`, `src/user/`, `src/xirc/mod.rs`; local-time logging in `src/logger.rs` and `src/server/configuration.rs`. Direct chrono/time-type usage spans 31 Rust files including tests at discovery.
- **Problem at discovery:** RustSec `RUSTSEC-2026-0543` reports all chrono versions unmaintained, with no patched upgrade. cargo-audit emits a warning; cargo-deny fails its advisory gate. Updating chrono to its latest version does not resolve this.
- **Impact:** Dependency maintenance risk; the original full dependency-policy check failed despite finding zero known vulnerabilities. STEP-030 resolves the maintenance advisory without ignoring it.
- **Recommended fix:** Replace chrono with **jiff**, as requested by the user. Inventory public timestamp types and conversions; preserve IRCv3 parsing of UTC timestamps with zero through nine fractional digits, canonical three-digit output, ordering, future clocks, duration bounds, local-time log output, and existing SQLite TEXT timestamps/cursors. Remove chrono only after GNU/musl and storage/protocol regression checks pass. Do not suppress the advisory to make the gate pass.
- **Verification:** STEP-030; server-time boundary/roundtrip tests, timestamp ordering and expiry tests, local-time logging fixtures, file-backed migration/reopen/FTS/cursor tests, full dual-runtime GNU/musl tests, release/static-link checks, and fresh cargo audit/deny/machete runs.
- **Status:** Completed in STEP-030. Migrated to Jiff 1.0.0 while preserving schema 35, canonical millisecond TEXT, nanosecond input and pre-epoch Unix topic rounding. Intentional leap-second and date-range differences are documented in [timestamp compatibility](dependencies.md#timestamp-compatibility). Required GNU/musl verification passes; audit has no vulnerabilities or advisory warnings, and full deny/machete pass. See [§15](progress.md#15-implementation-progress-log) for the complete verification record.
- **Evidence:** [Upstream maintenance announcement](https://github.com/chronotope/chrono/issues/1768); live cargo-audit 0.23.0 and cargo-deny 0.21.0 output against RustSec database commit `233155b4025b854c68a4b199c0e32d7fd53291a2`.

#### ARCH-001 — Cohesive files still contain oversized multi-responsibility handlers

- **Severity / classification:** Medium; confirmed maintainability concern, not a correctness defect by line count alone.
- **Location:** `src/downstream/mod.rs` (733 lines), `src/service/mod.rs` (666), `src/config.rs` (606), `src/downstream/cmds/history.rs:23`, `src/user/events.rs:134`, upstream `handle_message` functions; details in [§9](design-review.md#9-architecture-and-refactoring-opportunities).
- **Problem:** Command parsing, validation, state lookup, persistence and reply generation are often mixed in a single large function. `service::commands` also mixes a large declarative registry with dispatch implementation. Existing module splits are useful and should be retained.
- **Impact:** Harder to reason about validated state, cancellation, error contracts and protocol changes; duplicated state lookup drives unwrap/index diagnostics.
- **Recommended fix:** Extract typed history request parsing/query execution/reply formatting, service command registration, and cohesive per-command upstream handlers. Reduce user event dispatch to delegation. Use functions/modules around responsibilities, not arbitrary file-size limits.
- **Verification:** Existing transcript/dual-runtime/migration tests; add focused parsing/response tests before extraction; full Clippy with justified exceptions for declarative tables only where appropriate.
- **Status:** Completed for STEP-021–023. Implemented typed history request/query/reply boundaries, cohesive upstream/downstream command handlers, the typed service registry and ordered directive parser groups. Existing actor and READ dispatch remain cohesive ownership boundaries with local lint contracts; completion does not mean every function/file has a fixed size. Final 287-test GNU/musl suites, strict Clippy on both targets and strict rustdoc pass. Original/final Rust assertions and twelve pinned-ergo scenario comparisons preserve measured wire behavior; raw Go differences and fixture timing corrections are recorded in [§12](verification.md#12-verification-matrix).

#### TEST-001 — Adversarial lifecycle and native-code coverage is incomplete

- **Severity / classification:** Medium; confirmed missing coverage/tooling, not proof of unsafe behavior.
- **Location:** `tests/`, `src/conn/tests.rs`, `src/database/sqlite/tests/`, `.github/workflows/`; no fuzz workspace/property-test configuration found.
- **Problem:** Existing tests cover many regressions but not aggregate history budgets, abandoned connection ownership, final mention flush, push saturation, DB contention on one CPU, or autonomous command timeout. No continuous parser fuzzing run. Migrations have fixtures for old/current schemas but not exhaustive every-version/concurrent-process/crash coverage.
- **Impact:** Resource and cancellation regressions can pass the current suite; compiler/Clippy success does not validate native allocator/SQLite/AWS-LC behavior.
- **Recommended fix:** Add finding-specific regressions, property tests for parsers/IDs, separate fuzz targets/corpus, file-backed integrity/rollback/concurrent-initializer cases.
- **Verification:** Commands/coverage targets in phases 8 and 10. Preserve all existing tests; record unsupported tools as blocked rather than silently skipping.
- **Status:** Completed for STEP-024–025. Deterministic parser properties, bounded mutation corpora, transactional/migration/concurrent-initializer regressions and actual three-minute resource soaks pass. A WAL startup race found by the new tests is fixed with bounded retry. Final GNU/musl suites pass 298 tests each. Coverage-guided continuous fuzzing, browser/deployment comparison and multi-hour workload coverage remain explicit verification limits rather than inferred successes.

#### AUTH-001 — OAuth discovery requires issuer presence, not equality

- **Severity / classification:** Medium; confirmed missing validation; exploitation depends on provider/discovery trust conditions.
- **Location:** `src/auth.rs:270–324` (`OAuth2::new`), `src/auth.rs:361–380` (`discover`).
- **Problem:** Discovery metadata's issuer is checked only for nonemptiness. It is not checked against the configured issuer identity; the returned introspection endpoint is accepted without an explicit HTTPS/issuer policy. JSON response aggregation is not explicitly byte-bounded.
- **Impact:** Misconfigured/misdirected discovery can authenticate against an unintended issuer or send credentials to an unintended endpoint. The audit did not demonstrate interception of a correctly configured HTTPS issuer.
- **Recommended fix:** Apply the RFC 8414 issuer comparison rules, with carefully defined handling of configured userinfo and paths; explicitly define secure endpoint/redirect policy and any local-test exception. Bound metadata/introspection response bodies. Preserve generic external auth errors and never log client secrets/tokens.
- **Verification:** Mock discovery with matching/mismatching issuer, wrong endpoint scheme, redirect, oversized JSON, inactive/missing-username tokens. Document compatibility implications for existing providers. Reference: [RFC 8414 §3.3](https://www.rfc-editor.org/rfc/rfc8414#section-3.3).
- **Status:** Completed. Bind metadata issuer exactly to the credential-stripped configuration; require HTTPS and certificate validation, reject redirects and unsafe URL components, and bound both response bodies to 64 KiB with 10-second deadlines. Explicit loopback-only HTTP fixtures cannot delegate token posts to another origin. Redact HTTP/decoder and provider-username mismatch diagnostics. Matching/missing/mismatch, TLS/policy/redirect, known/chunked limits, malformed authorities, inactive/external PLAIN and echoed-credential regressions pass. Final GNU/musl 282-test suites, strict compiler/Clippy/rustdoc, release/static-link/CLI and dependency gates pass.

### 3.4 Low-Priority Issues

#### RUST-001 — Toolchain support is implicit

- **Severity / classification:** Low; confirmed policy/documentation gap.
- **Location:** `Cargo.toml:1–10`, README Building, both workflows' rolling distro setup.
- **Problem:** Edition 2024 does not specify the minimum compiler, and builds follow whatever stable version Arch supplies. The installed/compiler target already matches the requested Rust 1.100.
- **Impact:** Unclear supported compiler and reproducibility expectations.
- **Recommended fix:** Declare `rust-version = "1.100"` if adopting this audit's target as the support floor, or choose/test a justified lower MSRV. Record the tested stable version consistently; support distro Rust rather than require rustup unconditionally.
- **Verification:** Build/check on declared MSRV and current stable for GNU/musl. No `cargo fix --edition` needed.
- **Status:** Completed. Declared rust-version 1.100 with Edition 2024 unchanged; README and AGENTS now document the floor and distro/rustup support. Installed stable 1.100 passes fmt, check, Clippy and 213 tests on both GNU and musl, plus reader smoke and doc tests. CI Cargo commands enforce the manifest floor.

#### DOC-001 — Rustdoc has 13 warnings

- **Severity / classification:** Low; confirmed diagnostics.
- **Location:** `src/database/mod.rs:198,249`; `src/downstream/mod.rs:214,218,310`; `src/ircutil.rs:71`; `src/msgstore/bare.rs:1`; `src/proxyproto.rs:2`; `src/xirc/multiline.rs:230`.
- **Problem:** URL/grammar examples are interpreted as links/HTML and several URLs are bare. Default documentation succeeds but strict documentation fails.
- **Impact:** Broken rendered API docs and future CI failure.
- **Recommended fix:** Use inline code for grammar examples, proper Markdown/autolinks for URLs, and document public error/state contracts as lint remediation proceeds.
- **Verification:** `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked`.
- **Status:** Completed. Completed the prompt compiler/Clippy baseline, including pedantic, indexing/string-slice and error-mapping checks. Added API error/panic contracts, repaired rustdoc links/grammar, reused clones and preserved fixed protocol messages while retaining safe error causes. Reviewed statement-level bounds contracts and function-specific ordered-dispatch exceptions; ARCH-001 was deferred at STEP-011 completion and was subsequently addressed by STEP-021–023. No crate/module-wide lint suppression was added. fmt/check and strict GNU/musl all-target Clippy pass; all 233 tests per target plus reader smoke/doc tests pass. Strict rustdoc now passes with zero warnings.

#### CI-002 — Test inputs and toolchains float

- **Severity / classification:** Low; confirmed reproducibility risk.
- **Location:** `.github/workflows/*.yml` (`archlinux:latest`, mutable action tags); `tests/compat/setup.sh` (`SOJU_REF=master`, default-branch ergo/gamja, `npm install`).
- **Problem:** A later run can use different compiler/native/browser/upstream source versions for the same commit. Compatibility failures can reflect upstream changes. No freshness/advisory automation is configured.
- **Impact:** Harder reproduction and supply-chain review; not evidence of compromised dependencies/actions.
- **Recommended fix:** Record resolved tool/source versions, use pinned revisions and `npm ci` where a committed lockfile is provided, and separate pinned compatibility gates from scheduled latest-upstream exploration. Evaluate immutable action pins with automated updates.
- **Verification:** Two runs resolve the same pinned inputs; scheduled drift jobs report new revisions/advisories without weakening required checks.
- **Status:** Not Started; CI-hardening implementation is excluded by user direction. This finding remains open.

#### NUM-001 — Some public numeric boundaries silently reinterpret values

- **Severity / classification:** Low; confirmed unchecked conversions, potential correctness risk outside current x86-64 bounded protocol callers.
- **Location:** `src/database/sqlite/messages.rs:245,320` (`usize` limit to `i64`); `src/msgstore/db.rs:14–20` (message IDs); `src/msgstore/bare.rs:55` (decoded length to `usize`); `src/msgstore/memory.rs:45,67` (sequence arithmetic/cast); `src/rate.rs:44` (jitter nanoseconds).
- **Problem:** A limit above `i64::MAX` becomes negative (SQLite interprets negative LIMIT as unlimited); IDs/lengths can truncate or wrap; memory sequence exhaustion is unchecked. Current network history/search limits mitigate query overflow; no practical u64 sequence-exhaustion attack was established.
- **Impact:** Surprising public API behavior and portability/invariant hazards, especially for hypothetical 32-bit targets.
- **Recommended fix:** Use checked conversions or explicitly documented saturation where the API means unlimited; preserve intentional ZigZag bit encodings. Handle sequence exhaustion and validate nonzero/representable rate parameters. Do not claim 32-bit support without a build.
- **Verification:** Boundary tests for zero, maximum representable values, malformed overlong BARE integers, `usize::MAX`, and explicit sequence rollover handling; GNU/musl regression tests.
- **Status:** Completed. Enabled checked numeric-cast policy; replaced lossy BARE/SQLite/message-ID/source-address conversions, guarded memory sequence exhaustion, and validated rate configuration. Preserved Go ID vectors and documented explicit unlimited SQLite limits. PROXY input uses checked slices and patterns with focused indexing enforcement. fmt/check/strict Clippy and 227 GNU/musl tests plus reader smoke/doc tests pass. 32-bit support is not claimed; the internal indexing policy was completed in STEP-011.

#### PERF-001 — Receipt client enumeration is quadratic in distinct clients

- **Severity / classification:** Low; optional improvement, no measured bottleneck.
- **Location:** `src/user/delivered.rs:73–84` (`DeliveredStore::clients`), stop/receipt persistence paths in `src/user/events.rs`.
- **Problem:** Repeated `Vec::contains` deduplicates client names across target maps; retained historical client/target combinations have no explicit cache pruning policy.
- **Impact:** Growing disconnect/shutdown cost for unusually large histories of client names, plus retained receipt memory.
- **Recommended fix:** Measure representative receipt cardinalities; if warranted use a set for deduplication while preserving any externally relevant ordering, and design receipt-cache bounds without deleting durable cursors unexpectedly.
- **Verification:** Many-target/client benchmark; receipt replay and restart tests; demonstrate unchanged persistence semantics.
- **Status:** Completed. Bound persistent receipt dirty caches per user to 4,096 entries/4 MiB with write-through pressure handling, lazy 128-row reconnect pages, upsert-only partial flushes and failure-safe accounting. Preserve memory-only receipt lifetime and explicitly exclude it from whole-user memory claims. Seven-sample many-client benchmark documents small-client overhead and removes quadratic enumeration (4,096 clients: 393.526 ms to 3.052 ms). Shared pressure, 10k-row file reload, aliases/legacy bytes, failure/atomic initialization and reconnect/restart tests pass. Full GNU/musl checks (246 tests each), strict Clippy/rustdoc, cargo audit/deny/machete pass; schema 35 and durable rows preserved.

#### ASYNC-005 — Metric accounting is not cancellation-safe

- **Severity / classification:** Low; reproduced accounting defect.
- **Location:** `src/lib.rs:33–38` (`metrics_guard`); analogous upstream increments/decrements in `src/upstream/connect.rs:52–54`.
- **Problem:** Increment precedes an `.await`, decrement follows; dropping or unwinding the future skips the decrement. A temporary timeout probe leaves `downstreams` at 1 after its only guarded future is gone.
- **Impact:** Permanently inflated gauges after cancelled/unwinding sessions; no authentication or memory-safety impact asserted.
- **Recommended fix:** Use an RAII decrement guard and explicitly preserve the existing upstream count scope.
- **Verification:** Normal completion, cancellation and unwind tests where supported, all leaving the count at its initial value; STEP-003 tracks implementation.
- **Status:** Completed. Downstream and upstream counts now use one RAII guard; regressions cover completion, operational error, cancellation and unwind. fmt, check, default Clippy, 213 tests, reader smoke and doc tests pass. Rustdoc builds with the 13 pre-existing warnings.
