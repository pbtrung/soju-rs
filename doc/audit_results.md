# soju-rs — Comprehensive Rust Audit and Modernization

## 1. Executive Summary

Documentation maintenance: the audit and its prompt now live in `doc/`.
Source locations below are relative to the repository root. Audit findings,
verification results describe the 2026-11-17 audit baseline. Finding and
checklist statuses track subsequent implementation; documentation cleanup alone
does not mark code fixes complete.
The README's release and database descriptions have since been corrected, and
older review records moved to `doc/archive/`. Implementation progress and
remaining work are recorded by finding status and in §15.

Audit date: 2026-11-17. Reviewed commit: `c103b74be47920e307b99efe4ff739a1614ec7a1` (`soju` 0.99.25).

The original execution was **audit and planning only**, as required by [audit_prompt.md §17.5](audit_prompt.md#175-execution-order--mandatory), which superseded its earlier implementation instructions. No Rust source, manifests, lockfile, tests, release scripts, or workflows were changed. Finding statuses below track subsequent verified implementation. Build artifacts and temporary audit probes were generated; this report was that run's only new deliverable. The pre-existing untracked `audit_prompt.md` is preserved at `doc/audit_prompt.md`.

The baseline is healthy: formatting, all-target checking, default Clippy with warnings denied, and 210 tests pass on GNU; the same 210 tests pass on musl. Edition 2024 is already in use. Rust 1.100.0 is installed and its official release is verified. Default documentation generation succeeds with 13 warnings; documentation with warnings denied fails. The requested expanded lint policy fails with thousands of diagnostics and requires staged remediation.

The original priorities were broken release dispatch, missing automatic/test-before-release gates, unlimited history target retention, and unlimited notification task admission. Two smaller lifecycle defects were reproduced without changing the repository: cancellation leaks the downstream metric increment, and dropping connection handles leaves the writer/stream alive. The dependency inventory found `rand` 0.10.4 and `rusqlite` 0.43.0 newer than the declared/locked versions. No critical vulnerability, reachable undefined behavior, authentication bypass, or database corruption was established by this audit.

Scope and assurance: repository-wide inventory, static searches, compiler/Clippy analysis, targeted source tracing across security/resource/persistence paths, existing tests, release builds, and authoritative dependency research. This is not an exhaustive proof of every branch, dependency's internals, memory safety, or long-duration stability. Potential risks and optional improvements are explicitly identified. The prior [archived review](archive/code-audit.md) describes previous fixes; it is historical context, not evidence that this execution fixed anything.

## 2. Repository and Toolchain Baseline

| Item | Observed baseline |
| --- | --- |
| Package/workspace | Single package `soju` 0.99.25; library plus `soju` and `sojuctl` binaries |
| Rust source | 101 files under `src/`, 27,589 lines including embedded tests and benchmark |
| Edition | 2024 already; no edition migration needed |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09) (Arch Linux rust 1:1.100.0-1)` |
| Cargo | `cargo 1.100.0 (3acc79c0c 2026-11-05)` |
| MSRV/toolchain policy | No `rust-version`, toolchain file, or checked-in Cargo lint policy; README says latest stable |
| Toolchain management | Distro Rust; `rustup` unavailable. GNU/musl standard libraries and `musl-gcc` installed |
| Features | No package feature flags; 35 direct dependencies, Tokio also a dev dependency |
| Lock inventory | 270 packages including the local package; metadata contains platform-specific/optional graph entries |
| Native code | No application `unsafe` found. Bundled SQLite 3.54.1 via `libsqlite3-sys` 0.40.0, AWS-LC via `aws-lc-sys` 0.47.0, mimalloc via `libmimalloc-sys` 0.1.49 |
| Build scripts | No repository `build.rs`; native dependencies have their own build systems |
| Database | SQLite WAL, synchronous NORMAL, single connection behind a mutex; schema 35; migrations use an immediate transaction |
| Async architecture | User-owned mutable state, network runners, connection writers, input permits, timers and deferred tasks |
| CI | Two manually dispatched workflows; GNU/musl tests and release packaging exist separately |
| Original checkout | No tracked modifications; untracked `audit_prompt.md` (subsequently moved into `doc/`) |

Repository areas examined:

| Area | Files and review focus |
| --- | --- |
| Configuration/CLI | `Cargo.toml`, `Cargo.lock`, `config.in`, `src/config.rs`, `scfg.rs`, `flags.rs`, both binaries; runtime selection, directives, limits, startup/reload |
| Networking/security | `src/conn/`, `listen/`, `proxyproto.rs`, `tls.rs`, `auth.rs`, `certfp.rs`, `identd.rs`, `webpush.rs`; framing, trust boundaries, client authentication, pinning, size/time limits |
| IRC/state | `src/irc/`, `ircutil.rs`, `xirc/`, `downstream/`, `upstream/`, `user/`, `service/`; dispatch, authorization, SASL, batches, labels, reconnect, multiplexing, delivery |
| Persistence | `src/database/`, SQL schemas/indexes/triggers/migrations, `src/msgstore/`; transactions, scoped queries, ID allocation, retention, FTS, shutdown writes |
| Operations | `server/`, `logger.rs`, `metrics.rs`, `rate.rs`, `util.rs`; task ownership, lock use, CPU admission, shutdown, observability |
| Tests/tooling | Unit modules, `tests/server*`, `tests/listeners.rs`, all compatibility scripts, Playwright test/config, reader benchmark and Python benchmark |
| Delivery/docs | Both workflows, all release scripts, README and `doc/`; release ordering, compatibility claims, GNU/musl deployment and existing performance evidence |

Baseline commands and actual outcomes are in §12. Socket tests initially failed under the sandbox (`Operation not permitted`); rerunning with approved local socket access passed. These environmental failures are not application defects.

## 3. Audit Findings

Status denotes **implementation status**, not whether investigation is complete. All remediation is **Not Started** under this audit-only instruction. Severity reflects practical impact; an unbounded structure does not by itself establish a remotely demonstrated denial of service.

### 3.1 Critical Issues

None confirmed. Advisory scanning, fuzzing, sanitizers and long-running load testing remain outstanding; absence of a critical finding is not a security certification.

### 3.2 High-Priority Issues

#### REL-001 — Release helpers dispatch a nonexistent workflow

- **Severity / classification:** High; confirmed operational defect by repository inspection.
- **Location:** `scripts/release-new.sh:92`, `scripts/release-replace.sh:52–58`; existing workflows are `.github/workflows/tests.yml` and `release.yml`.
- **Problem:** Both helpers dispatch `ci.yml`, which is absent. The replacement helper first pushes the branch, moves the remote tag and can delete the existing release/assets. Dispatch then fails against this repository configuration. README also describes a combined CI/release workflow that no longer exists.
- **Impact:** New release automation fails; replacement can remove the current release without starting its replacement. No remote mutation was performed in this audit.
- **Recommended fix:** Establish the final tested release workflow, update both dispatches and documentation, and preflight workflow availability before any destructive operation. Add local stub-based script tests that record command ordering and simulate missing workflows/API failures. Preserve leases and clean-checkout guards.
- **Verification:** `bash -n scripts/release-*.sh`; hermetic shell test with stub `git`/`gh`; ensure failed preflight performs no push, tag move or delete; verify dispatched filename exists and accepts `publish`.
- **Status:** Completed. Release helpers now preflight the active remote release.yml against the local definition before mutation; six hermetic regression tests and bash syntax checks pass. Ruff and ShellCheck are unavailable. CI-001 remains open for phase 9.

#### CI-001 — Tests are manual and publishing does not require them

- **Severity / classification:** High; confirmed enforcement gap.
- **Location:** `.github/workflows/tests.yml:4–5,56–60`; `.github/workflows/release.yml:4–5,18–103`.
- **Problem:** No push or pull-request trigger. The publish job needs only the archive build. That build runs neither the Rust tests nor the compatibility tests. No workflow enforces formatting, Clippy, rustdoc warnings or the full requested quality gates.
- **Impact:** Regressions can merge or ship despite failing tests. README's claim that tests pass before publishing is unsupported by the workflow graph.
- **Recommended fix:** Add automatic checks and make the release build/publish graph depend on checks of the same commit. Use a reusable test workflow or equivalent dependency; retain explicit publishing authorization and minimal permissions.
- **Verification:** Inspect job dependencies and event triggers; use a local workflow validator, then in a separately authorized CI run demonstrate that a failing check prevents artifact publication.
- **Status:** Not Started.

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
- **Status:** Not Started.

#### ASYNC-003 — Delayed mention persistence is not drained at shutdown

- **Severity / classification:** Medium; confirmed lifecycle gap; end-to-end loss regression still required.
- **Location:** `src/upstream/messages.rs:369–409` (`record_mention`); `src/user/events.rs:350–367`; `src/server.rs:238–250`.
- **Problem:** Mentions wait one second in a detached task. Stop closes connections and saves delivery receipts, but does not flush/join `pending_mentions`. Server shutdown waits only for users; runtime teardown may cancel the delayed task before its DB write. Taking the pending map before a failed write also loses its retry data.
- **Impact:** A recently received message can remain in history without its mention marker after orderly restart, or after transient persistence failure.
- **Recommended fix:** Make mention flushing user/network-owned, preserve batches until successful persistence, and drain them before signalling user completion. Define retry/backoff and deletion behavior; avoid writing markers for a deleted network.
- **Verification:** Receive a mention then stop before one second; reopen a file DB and assert marker persisted. Inject write failure then recovery, and delete network/user during a pending flush.
- **Status:** Completed. Replace detached mention flushes with actor-owned maps/deadlines and joined blocking batch transactions. Clear only after commit, retain/merge failures and retry autonomously; stop flushes all batches with three attempts and explicit failure metrics/logs. Network updates transfer state, deletion is serialized and foreign keys prevent stale resurrection. Stop-before-delay/reopen, rollback/recovery, monotonic/delete-during-flush and permanent-error tests pass. Full GNU/musl checks (269 tests each), strict Clippy/rustdoc and documentation checks pass. STEP-019 remains for handler responsiveness.

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
- **Status:** Not Started.

#### LINT-001 — Required correctness policy is absent and fails when trialled

- **Severity / classification:** Medium; confirmed policy gap/diagnostics, not thousands of verified bugs.
- **Location at discovery:** `Cargo.toml` (no lint tables), `src/conn/reader.rs:49`, `src/upstream/mod.rs:35–39`, protocol handlers, tests and BARE/message-ID conversions.
- **Problem at discovery:** Default Clippy passes; enabling pedantic plus requested restriction lints fails. The trial reported 1,773 errors for the library and 2,453 for the lib-test target; these overlap and must not be summed as unique defects. Examples include indexing, unwrap/expect, unchecked casts, missing contracts and oversized functions.
- **Impact:** Desired invariants are not mechanically enforced. Many diagnostics represent checked local invariants/test assertions and need a reasoned disposition, not arbitrary fallback values.
- **Recommended fix:** Stage explicit Cargo rust/Clippy policies; use checked conversion/borrowed-state APIs and typed variants; document small unavoidable invariant/test exceptions with precise `expect` reasons. Never enable all restriction lints. Include unused-must-use and unsafe prohibition from the prompt.
- **Verification:** Full all-target Clippy command with the final manifest policy and `-D warnings`; check exception reasons and unfulfilled expectations on Rust 1.100. Re-run behavior tests after each logical group.
- **Status:** Completed. Completed the prompt compiler/Clippy baseline, including pedantic, indexing/string-slice and error-mapping checks. Added API error/panic contracts, repaired rustdoc links/grammar, reused clones and preserved fixed protocol messages while retaining safe error causes. Reviewed statement-level bounds contracts and function-specific ordered-dispatch exceptions; ARCH-001 extraction remains a separate Phase-7 task and is not claimed resolved. No crate/module-wide lint suppression was added. fmt/check and strict GNU/musl all-target Clippy pass; all 233 tests per target plus reader smoke/doc tests pass. Strict rustdoc now passes with zero warnings.

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
- **Problem:** Registry live results show 0.10.4 and 0.43.0 respectively. Upstream rusqlite release notes include memory-safety fixes in APIs outside the explicitly enabled application feature set; assess feature reachability instead of assigning a vulnerability to every use. Transitive latest-version differences appear in §5.
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
- **Status:** Completed in STEP-030. Migrated to Jiff 1.0.0 while preserving schema 35, canonical millisecond TEXT, nanosecond input and pre-epoch Unix topic rounding. Intentional leap-second and date-range differences are documented in [timestamp compatibility](dependencies.md#timestamp-compatibility). Required GNU/musl verification passes; audit has no vulnerabilities or advisory warnings, and full deny/machete pass. See §15 for the complete verification record.
- **Evidence:** [Upstream maintenance announcement](https://github.com/chronotope/chrono/issues/1768); live cargo-audit 0.23.0 and cargo-deny 0.21.0 output against RustSec database commit `233155b4025b854c68a4b199c0e32d7fd53291a2`.

#### ARCH-001 — Cohesive files still contain oversized multi-responsibility handlers

- **Severity / classification:** Medium; confirmed maintainability concern, not a correctness defect by line count alone.
- **Location:** `src/downstream/mod.rs` (733 lines), `src/service/mod.rs` (666), `src/config.rs` (606), `src/downstream/cmds/history.rs:23`, `src/user/events.rs:134`, upstream `handle_message` functions; details in §9.
- **Problem:** Command parsing, validation, state lookup, persistence and reply generation are often mixed in a single large function. `service::commands` also mixes a large declarative registry with dispatch implementation. Existing module splits are useful and should be retained.
- **Impact:** Harder to reason about validated state, cancellation, error contracts and protocol changes; duplicated state lookup drives unwrap/index diagnostics.
- **Recommended fix:** Extract typed history request parsing/query execution/reply formatting, service command registration, and cohesive per-command upstream handlers. Reduce user event dispatch to delegation. Use functions/modules around responsibilities, not arbitrary file-size limits.
- **Verification:** Existing transcript/dual-runtime/migration tests; add focused parsing/response tests before extraction; full Clippy with justified exceptions for declarative tables only where appropriate.
- **Status:** Not Started.

#### TEST-001 — Adversarial lifecycle and native-code coverage is incomplete

- **Severity / classification:** Medium; confirmed missing coverage/tooling, not proof of unsafe behavior.
- **Location:** `tests/`, `src/conn/tests.rs`, `src/database/sqlite/tests/`, `.github/workflows/`; no fuzz workspace/property-test/sanitizer/Miri configuration found.
- **Problem:** Existing tests cover many regressions but not aggregate history budgets, abandoned connection ownership, final mention flush, push saturation, DB contention on one CPU, or autonomous command timeout. No continuous parser fuzzing or native sanitizer run. Migrations have fixtures for old/current schemas but not exhaustive every-version/concurrent-process/crash coverage.
- **Impact:** Resource and cancellation regressions can pass the current suite; compiler/Clippy success does not validate native allocator/SQLite/AWS-LC behavior.
- **Recommended fix:** Add finding-specific regressions, property tests for parsers/IDs, separate fuzz targets/corpus, file-backed integrity/rollback/concurrent-initializer cases, and supported scheduled native sanitizer jobs. Restrict Miri to suitable pure Rust tests if dependencies cannot run in its interpreter.
- **Verification:** Commands/coverage targets in phases 8–10. Preserve all existing tests; record unsupported tools as blocked rather than silently skipping.
- **Status:** Not Started.

#### AUTH-001 — OAuth discovery requires issuer presence, not equality

- **Severity / classification:** Medium; confirmed missing validation; exploitation depends on provider/discovery trust conditions.
- **Location:** `src/auth.rs:270–324` (`OAuth2::new`), `src/auth.rs:361–380` (`discover`).
- **Problem:** Discovery metadata's issuer is checked only for nonemptiness. It is not checked against the configured issuer identity; the returned introspection endpoint is accepted without an explicit HTTPS/issuer policy. JSON response aggregation is not explicitly byte-bounded.
- **Impact:** Misconfigured/misdirected discovery can authenticate against an unintended issuer or send credentials to an unintended endpoint. The audit did not demonstrate interception of a correctly configured HTTPS issuer.
- **Recommended fix:** Apply the RFC 8414 issuer comparison rules, with carefully defined handling of configured userinfo and paths; explicitly define secure endpoint/redirect policy and any local-test exception. Bound metadata/introspection response bodies. Preserve generic external auth errors and never log client secrets/tokens.
- **Verification:** Mock discovery with matching/mismatching issuer, wrong endpoint scheme, redirect, oversized JSON, inactive/missing-username tokens. Document compatibility implications for existing providers. Reference: [RFC 8414 §3.3](https://www.rfc-editor.org/rfc/rfc8414#section-3.3).
- **Status:** Not Started.

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
- **Status:** Completed. Completed the prompt compiler/Clippy baseline, including pedantic, indexing/string-slice and error-mapping checks. Added API error/panic contracts, repaired rustdoc links/grammar, reused clones and preserved fixed protocol messages while retaining safe error causes. Reviewed statement-level bounds contracts and function-specific ordered-dispatch exceptions; ARCH-001 extraction remains a separate Phase-7 task and is not claimed resolved. No crate/module-wide lint suppression was added. fmt/check and strict GNU/musl all-target Clippy pass; all 233 tests per target plus reader smoke/doc tests pass. Strict rustdoc now passes with zero warnings.

#### CI-002 — Test inputs and toolchains float

- **Severity / classification:** Low; confirmed reproducibility risk.
- **Location:** `.github/workflows/*.yml` (`archlinux:latest`, mutable action tags); `tests/compat/setup.sh` (`SOJU_REF=master`, default-branch ergo/gamja, `npm install`).
- **Problem:** A later run can use different compiler/native/browser/upstream source versions for the same commit. Compatibility failures can reflect upstream changes. No freshness/advisory automation is configured.
- **Impact:** Harder reproduction and supply-chain review; not evidence of compromised dependencies/actions.
- **Recommended fix:** Record resolved tool/source versions, use pinned revisions and `npm ci` where a committed lockfile is provided, and separate pinned compatibility gates from scheduled latest-upstream exploration. Evaluate immutable action pins with automated updates.
- **Verification:** Two runs resolve the same pinned inputs; scheduled drift jobs report new revisions/advisories without weakening required checks.
- **Status:** Not Started.

#### NUM-001 — Some public numeric boundaries silently reinterpret values

- **Severity / classification:** Low; confirmed unchecked conversions, potential correctness risk outside current x86-64 bounded protocol callers.
- **Location:** `src/database/sqlite/messages.rs:245,320` (`usize` limit to `i64`); `src/msgstore/db.rs:14–20` (message IDs); `src/msgstore/bare.rs:55` (decoded length to `usize`); `src/msgstore/memory.rs:45,67` (sequence arithmetic/cast); `src/rate.rs:44` (jitter nanoseconds).
- **Problem:** A limit above `i64::MAX` becomes negative (SQLite interprets negative LIMIT as unlimited); IDs/lengths can truncate or wrap; memory sequence exhaustion is unchecked. Current network history/search limits mitigate query overflow; no practical u64 sequence-exhaustion attack was established.
- **Impact:** Surprising public API behavior and portability/invariant hazards, especially for hypothetical 32-bit targets.
- **Recommended fix:** Use checked conversions or explicitly documented saturation where the API means unlimited; preserve intentional ZigZag bit encodings. Handle sequence exhaustion and validate nonzero/representable rate parameters. Do not claim 32-bit support without a build.
- **Verification:** Boundary tests for zero, maximum representable values, malformed overlong BARE integers, `usize::MAX`, and explicit sequence rollover handling; GNU/musl regression tests.
- **Status:** Completed. Enabled checked numeric-cast policy; replaced lossy BARE/SQLite/message-ID/source-address conversions, guarded memory sequence exhaustion, and validated rate configuration. Preserved Go ID vectors and documented explicit unlimited SQLite limits. PROXY input uses checked slices and patterns with focused indexing enforcement. fmt/check/strict Clippy and 227 GNU/musl tests plus reader smoke/doc tests pass. 32-bit support is not claimed; remaining internal indexing policy is staged for STEP-011.

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

## 4. Rust 1.100 and Edition 2024 Modernization

Original and audited compiler are the same: **1.100.0**. Original and proposed edition are **2024**. The [official stable release notes](https://doc.rust-lang.org/stable/releases.html) list Rust 1.100.0 released on 2026-11-12; the local compiler identifies the matching commit. This run therefore has no unavailable-1.100 limitation. No compiler installation or edition rewrite was performed.

The code already uses let chains, let-else, `Option::is_some_and`/`is_none_or`, `OnceLock`, checked message-ID allocation and saturating backoff. Retain those readable idioms. Prefer `TryFrom`, pattern matching and borrowing validated state over index/unwrap pairs. Use a small RAII guard for metric/resource cleanup; evaluate the new standard drop-guard facilities only if they simplify the actual ownership contract. Do not introduce custom allocators, never-type rewrites or new syntax merely because 1.100 provides them. No deprecated API warning was observed in the normal compiler/Clippy baseline.

Proposed policy: explicitly document the support floor, test stable/MSRV, and leave production stable-only. Read the exact target release notes before selecting APIs. Reference: [Edition 2024 guide](https://doc.rust-lang.org/edition-guide/rust-2024/index.html).

## 5. Dependency Upgrade Assessment

The completed upgrade/feature review and all 34 retained parent-version
constraints are documented in [dependency maintenance](dependencies.md).
The original dated inventory below remains for comparison.

STEP-006 migration review: rusqlite 0.43.0 changes empty/comment-only statement
handling and fixes optional APIs described in its upstream release notes.
No application API adaptation is required. Bundled `libsqlite3-sys` remains
0.40.0 with SQLite 3.54.1; schema 35, native configuration and database format
are unchanged. A new file-backed schema-30 migration/reopen regression verifies
history and cursor preservation, non-reused IDs, SQLite/foreign-key integrity
and FTS external-content integrity. All existing tests remain present.

Implementation follow-up (STEP-005): a live `cargo update --dry-run` refreshed
the index and identified seven compatible stable updates, now applied for
verification: bumpalo 3.20.4, cc 1.11.2, errno 0.3.15, mio 1.2.5,
rand 0.10.4, rustls-webpki 0.103.17 and zeroize 1.9.2. The tables below retain
the original audit snapshot; §15 records verified implementation outcomes.
The rusqlite 0.43 migration is deliberately isolated in STEP-006.

The first live RustSec scan (cargo-audit 0.23.0, database commit
`233155b4025b854c68a4b199c0e32d7fd53291a2`, 1,527 advisories) reports zero
known vulnerabilities and the informational unmaintained-chrono advisory
`RUSTSEC-2026-0543`. No advisory was suppressed. STEP-007 assessed the migration
needed to replace chrono; a passing audit exit code does not mean no warnings.
STEP-030 subsequently replaces it with Jiff 1.0.0; the refreshed scan has no
vulnerabilities or advisory warnings and the full deny policy passes.

All 35 direct dependencies and every registry package name in Cargo.lock were queried against the live crates.io API during this run; no API query errors remained. Stable, non-yanked versions are the comparison target. These results are a dated inventory, **not proof that each latest transitive can satisfy its parents**. No upgrade was applied during the original audit; implementation updates are recorded above.

Source for each row: `https://crates.io/api/v1/crates/<crate>` (linked below). The full raw query snapshot is temporarily in `/tmp/soju-audit-deps.json`; this report preserves actionable version differences so that implementation does not depend on that temporary file.

| Direct crate | Declared / locked | Latest stable (2026-11-17) | Assessment |
| --- | --- | --- | --- |
| [aes-gcm](https://crates.io/api/v1/crates/aes-gcm) | 0.11.1 / 0.11.1 | 0.11.1 | Current |
| [anyhow](https://crates.io/api/v1/crates/anyhow) | 1.0.104 / 1.0.104 | 1.0.104 | Current |
| [aws-lc-rs](https://crates.io/api/v1/crates/aws-lc-rs) | 1.19.0 / 1.19.0 | 1.19.0 | Current |
| [base64](https://crates.io/api/v1/crates/base64) | 0.23.1 / 0.23.1 | 0.23.1 | Current |
| [bcrypt](https://crates.io/api/v1/crates/bcrypt) | 0.19.3 / 0.19.3 | 0.19.3 | Current |
| [chrono](https://crates.io/api/v1/crates/chrono) | 0.4.45 / 0.4.45 | 0.4.45 | Current |
| [futures-util](https://crates.io/api/v1/crates/futures-util) | 0.3.34 / 0.3.34 | 0.3.34 | Current |
| [gethostname](https://crates.io/api/v1/crates/gethostname) | 1.1.0 / 1.1.0 | 1.1.0 | Current |
| [hex](https://crates.io/api/v1/crates/hex) | 0.4.3 / 0.4.3 | 0.4.3 | Current |
| [hkdf](https://crates.io/api/v1/crates/hkdf) | 0.13.0 / 0.13.0 | 0.13.0 | Current |
| [http-body-util](https://crates.io/api/v1/crates/http-body-util) | 0.1.5 / 0.1.5 | 0.1.5 | Current |
| [hyper](https://crates.io/api/v1/crates/hyper) | 1.12.0 / 1.12.0 | 1.12.0 | Current |
| [hyper-util](https://crates.io/api/v1/crates/hyper-util) | 0.1.21 / 0.1.21 | 0.1.21 | Current |
| [indexmap](https://crates.io/api/v1/crates/indexmap) | 2.14.2 / 2.14.2 | 2.14.2 | Current |
| [ipnet](https://crates.io/api/v1/crates/ipnet) | 2.12.2 / 2.12.2 | 2.12.2 | Current |
| [libc](https://crates.io/api/v1/crates/libc) | 0.2.191 / 0.2.191 | 0.2.191 | Current |
| [mimalloc](https://crates.io/api/v1/crates/mimalloc) | 0.1.52 / 0.1.52 | 0.1.52 | Current |
| [p256](https://crates.io/api/v1/crates/p256) | 0.14.0 / 0.14.0 | 0.14.0 | Current |
| [rand](https://crates.io/api/v1/crates/rand) | 0.10.3 / 0.10.3 | 0.10.4 | Patch upgrade candidate |
| [rcgen](https://crates.io/api/v1/crates/rcgen) | 0.14.10 / 0.14.10 | 0.14.10 | Current |
| [reqwest](https://crates.io/api/v1/crates/reqwest) | 0.13.5 / 0.13.5 | 0.13.5 | Current |
| [rusqlite](https://crates.io/api/v1/crates/rusqlite) | 0.42.0 / 0.42.0 | 0.43.0 | Semver-breaking pre-1.0 upgrade; dedicated migration |
| [rustls](https://crates.io/api/v1/crates/rustls) | 0.23.46 / 0.23.46 | 0.23.46 | Current |
| [rustls-native-certs](https://crates.io/api/v1/crates/rustls-native-certs) | 0.8.5 / 0.8.5 | 0.8.5 | Current |
| [serde](https://crates.io/api/v1/crates/serde) | 1.0.229 / 1.0.229 | 1.0.229 | Current |
| [serde_json](https://crates.io/api/v1/crates/serde_json) | 1.0.154 / 1.0.154 | 1.0.154 | Current |
| [sha1](https://crates.io/api/v1/crates/sha1) | 0.11.0 / 0.11.0 | 0.11.0 | Current |
| [sha2](https://crates.io/api/v1/crates/sha2) | 0.11.1 / 0.11.1 | 0.11.1 | Current |
| [socket2](https://crates.io/api/v1/crates/socket2) | 0.6.5 / 0.6.5 | 0.6.5 | Current |
| [time](https://crates.io/api/v1/crates/time) | 0.3.55 / 0.3.55 | 0.3.55 | Current |
| [tokio](https://crates.io/api/v1/crates/tokio) | 1.54.1 / 1.54.1 | 1.54.1 | Current |
| [tokio-rustls](https://crates.io/api/v1/crates/tokio-rustls) | 0.26.6 / 0.26.6 | 0.26.6 | Current |
| [tokio-tungstenite](https://crates.io/api/v1/crates/tokio-tungstenite) | 0.30.0 / 0.30.0 | 0.30.0 | Current |
| [tokio-util](https://crates.io/api/v1/crates/tokio-util) | 0.7.20 / 0.7.20 | 0.7.20 | Current |
| [webpki-roots](https://crates.io/api/v1/crates/webpki-roots) | 1.0.12 / 1.0.12 | 1.0.12 | Current |

`rusqlite` 0.42 → 0.43 is a **semver-breaking pre-1.0 minor** migration, equivalent to a major compatibility review, not a patch-only update. Its [official 0.43 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.43.0) changes empty/comment-only SQL handling and fixes issues in changeset streaming, column metadata, virtual-table value handling and time conversion. The resolved features here are `bundled`, `cache`, default/cache support, `modern_sqlite` and the wasm binding support feature; session, column_metadata, vtab and time are not enabled. The cited fixes therefore do not by themselves establish a reachable vulnerability in soju-rs. Recheck feature reachability with advisory tooling. Version 0.43 declares Rust 1.89; rand 0.10.4 declares 1.85, both within the installed compiler's support.

All remaining locked-to-latest differences (excluding direct crates already above):

| Locked registry package | Locked | Latest stable | Assessment |
| --- | --- | --- | --- |
| [asn1-rs-derive](https://crates.io/api/v1/crates/asn1-rs-derive) | 0.6.0 | 0.7.0 | Review parent/target constraint before upgrade |
| [asn1-rs-impl](https://crates.io/api/v1/crates/asn1-rs-impl) | 0.2.0 | 0.3.0 | Review parent/target constraint before upgrade |
| [bit-vec](https://crates.io/api/v1/crates/bit-vec) | 0.9.1 | 0.10.2 | Review parent/target constraint before upgrade |
| [bumpalo](https://crates.io/api/v1/crates/bumpalo) | 3.20.3 | 3.20.4 | Patch candidate; refresh index and resolve |
| [cc](https://crates.io/api/v1/crates/cc) | 1.11.1 | 1.11.2 | Patch candidate; refresh index and resolve |
| [errno](https://crates.io/api/v1/crates/errno) | 0.3.14 | 0.3.15 | Patch candidate; refresh index and resolve |
| [getrandom](https://crates.io/api/v1/crates/getrandom) | 0.2.17 | 0.4.3 | Review parent/target constraint before upgrade |
| [lock_api](https://crates.io/api/v1/crates/lock_api) | 0.4.14 | 0.5.0 | Review parent/target constraint before upgrade |
| [mio](https://crates.io/api/v1/crates/mio) | 1.2.4 | 1.2.5 | Patch candidate; refresh index and resolve |
| [nom](https://crates.io/api/v1/crates/nom) | 7.1.3 | 8.0.0 | Review parent/target constraint before upgrade |
| [num-bigint](https://crates.io/api/v1/crates/num-bigint) | 0.4.8 | 0.5.1 | Review parent/target constraint before upgrade |
| [parking_lot](https://crates.io/api/v1/crates/parking_lot) | 0.12.5 | 0.13.0 | Review parent/target constraint before upgrade |
| [parking_lot_core](https://crates.io/api/v1/crates/parking_lot_core) | 0.9.12 | 0.10.0 | Review parent/target constraint before upgrade |
| [quinn-udp](https://crates.io/api/v1/crates/quinn-udp) | 0.5.16 | 0.6.3 | Review parent/target constraint before upgrade |
| [r-efi](https://crates.io/api/v1/crates/r-efi) | 6.0.0 | 7.2.0 | Platform-specific; resolve through parent |
| [redox_syscall](https://crates.io/api/v1/crates/redox_syscall) | 0.5.18 | 0.9.5 | Platform-specific; resolve through parent |
| [rusticata-macros](https://crates.io/api/v1/crates/rusticata-macros) | 4.1.0 | 5.0.0 | Review parent/target constraint before upgrade |
| [rustls-webpki](https://crates.io/api/v1/crates/rustls-webpki) | 0.103.16 | 0.103.17 | Patch candidate; refresh index and resolve |
| [syn](https://crates.io/api/v1/crates/syn) | 2.0.119 | 3.0.10 | Review parent/target constraint before upgrade |
| [synstructure](https://crates.io/api/v1/crates/synstructure) | 0.13.2 | 0.14.0 | Review parent/target constraint before upgrade |
| [tower-http](https://crates.io/api/v1/crates/tower-http) | 0.6.11 | 0.7.1 | Review parent/target constraint before upgrade |
| [untrusted](https://crates.io/api/v1/crates/untrusted) | 0.7.1 | 0.9.0 | Review parent/target constraint before upgrade |
| [wasi](https://crates.io/api/v1/crates/wasi) | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | Platform-specific; resolve through parent |
| [windows-core](https://crates.io/api/v1/crates/windows-core) | 0.62.2 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-implement](https://crates.io/api/v1/crates/windows-implement) | 0.60.2 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-interface](https://crates.io/api/v1/crates/windows-interface) | 0.59.3 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-link](https://crates.io/api/v1/crates/windows-link) | 0.2.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-result](https://crates.io/api/v1/crates/windows-result) | 0.4.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-strings](https://crates.io/api/v1/crates/windows-strings) | 0.5.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-sys](https://crates.io/api/v1/crates/windows-sys) | 0.52.0 | 0.61.2 | Platform-specific; resolve through parent |
| [windows-targets](https://crates.io/api/v1/crates/windows-targets) | 0.52.6 | 0.53.5 | Platform-specific; resolve through parent |
| [windows_aarch64_gnullvm](https://crates.io/api/v1/crates/windows_aarch64_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_aarch64_msvc](https://crates.io/api/v1/crates/windows_aarch64_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_gnu](https://crates.io/api/v1/crates/windows_i686_gnu) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_gnullvm](https://crates.io/api/v1/crates/windows_i686_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_msvc](https://crates.io/api/v1/crates/windows_i686_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_gnu](https://crates.io/api/v1/crates/windows_x86_64_gnu) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_gnullvm](https://crates.io/api/v1/crates/windows_x86_64_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_msvc](https://crates.io/api/v1/crates/windows_x86_64_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [zeroize](https://crates.io/api/v1/crates/zeroize) | 1.9.1 | 1.9.2 | Patch candidate; refresh index and resolve |

Compatible candidates include bumpalo, cc, errno, mio, rustls-webpki and zeroize patch updates. Parent-constrained breaking lines include parking_lot/lock_api, tower-http, nom, syn, ASN.1/Windows/WASI dependencies and older getrandom/untrusted. Some entries are platform-only or optional rather than part of the tested Linux feature graph. Upgrade parents where supported, or document the constraint; do not require every package to share its global latest release. Default active-target `cargo tree --duplicates` reports only `untrusted` 0.7.1 (AWS-LC) and 0.9.0 (webpki); neither duplication alone establishes waste or a vulnerability.

`cargo update --dry-run --offline` found zero updates using the **cached index**. This does not contradict the live API: it shows why a refreshed index is required before the implementation refresh. Neither Cargo.lock nor manifests changed during that original dry run. At audit time `cargo outdated`, `cargo audit`, `cargo deny`, and `cargo machete` were unavailable. Current implementation results are in §12: audit/deny/machete were subsequently installed; cargo-outdated remains absent. No memory-safety proof or advisory suppression is claimed. Direct source usage was reviewed; no dependency was proven safely removable. Tokio's broad `full` feature merits a later feature audit, not an untested feature reduction.

Native assessment: application code has no unsafe/FFI calls to justify bypassing safety checks. Native code remains in SQLite, AWS-LC and mimalloc. Both deployment targets must retain native build/static-link checks. Keep the current allocator until representative memory measurements justify a change. Dependency freshness is not a substitute for an advisory/native security review.

## 6. Compiler and Clippy Hardening

Default command passes:

```sh
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The following diagnostic trial actually ran and **failed**:

```sh
cargo clippy --workspace --all-targets --all-features --locked -- \
  -D warnings -W clippy::pedantic -W clippy::unwrap_used \
  -W clippy::expect_used -W clippy::panic -W clippy::indexing_slicing \
  -W clippy::string_slice -W clippy::cast_possible_truncation \
  -W clippy::cast_possible_wrap -W clippy::cast_sign_loss \
  -W clippy::let_underscore_must_use -W clippy::let_underscore_future \
  -W clippy::map_err_ignore -W clippy::allow_attributes
```

Compiler summaries: 1,773 lib diagnostics and 2,453 lib-test diagnostics, with overlapping code. Text-log link occurrences include 924 indexing, 685 unwrap, 142 must-use candidate, 121 missing-errors-doc, 81 doc-Markdown, 38 ignored must-use, 33 string-slice and 23 too-many-lines diagnostics. These are diagnostic occurrences, not unique defects, and other targets can reveal further issues after the library is remediated.

Apply the prompt's rust lint baseline (`unsafe_code = forbid`; deny unused must-use/imports/variables, unreachable code and unfulfilled expectations) and Clippy all/pedantic with negative group priorities, then its selected restriction lints. Explicitly include todo/unimplemented/dbg prohibitions even though the trial focused on observed categories. Audit each ignored send result: a receiver intentionally gone at shutdown differs from lost mandatory persistence. Test fixture unwraps can receive narrowly scoped, explained exceptions, or propagate fixture setup errors; do not weaken the entire production crate for tests. Guarded byte indexing is not automatically a bug; prefer `get`, slice patterns and `split_first` where they communicate the check cleanly.

Use the [Clippy lint reference for Rust 1.100](https://rust-lang.github.io/rust-clippy/rust-1.100.0/index.html) and local compiler support. No blanket restriction group or unexplained suppression is proposed.

## 7. Memory Safety and Resource Management

Existing protections worth preserving: 32 KiB input framing cap; 16 MiB accounted physical output queue; stricter 256 KiB unregistered output threshold; 64 ordinary plus four keepalive input permits; 64 pending commands per type; 64 open upstream batches; 64 KiB/1,024-line multiline cap; bounded CPU-heavy work and Web Push registration checks; handshake/registration/write/HTTP timeouts; large line-buffer capacity release; network cleanup of memory history. An unbounded Tokio channel with an enforced byte budget is not automatically an unlimited output buffer.

Remaining gaps: aggregate memory history/receipt cardinality, push tasks, accepted sessions, and writer teardown (MEM-001/MEM-002/ASYNC-001/ASYNC-004). Connection byte accounting uses atomics but does not prove a process-wide bound; message formatting allocates before admission and concurrent sends can transiently allocate above the queued budget. Stress/race tests should measure the practical envelope rather than claim exact RSS from an atomic counter.

No application unsafe code, manual pointer arithmetic or FFI boundary was found. Safe Rust does not prevent the resource exhaustion paths above. Native memory-safety coverage requires supported sanitizers; Miri cannot certify arbitrary native SQLite/AWS-LC/mimalloc calls.

Temporary public-API probes compiled with the current library produced:

```text
cancelled_guard=true downstream_gauge=1
drop_all_connection_handles_peer_read=Err(Elapsed(()))
first_target_retained_after_10000_targets=true initially_empty=true
```

The first poll was cancelled by timeout; the correct gauge after cancellation should be zero. The connection probe uses `tokio::io::duplex`, drops both returned handles and waits for peer EOF. The history probe appends one message to 10,000 unique targets and verifies the oldest ring remains. These are temporary audit probes, not checked-in regression tests, RSS measurements or fixes. Their outcomes must be converted into meaningful regression tests in implementation.

## 8. Async and Concurrency Correctness

The actor design keeps most protocol maps single-owner; retain that benefit. Registered socket input carries owned permits into user events, including across partial-read cancellation. Network runner IDs/connection IDs reject stale events. CPU work retains its permit through non-cancellable `spawn_blocking`, which is appropriate. Network timers have explicit abort handles on normal replacement/disconnection/stop paths. No lock-across-await deadlock was confirmed in the inspected shared-state paths.

The main ownership gaps are detached push/mention/writer/maintenance/accept tasks and completion scope: `Server::shutdown` waits for user tasks but does not join every task spawned on their behalf. `spawn_blocking` jobs that already started cannot be aborted merely by cancelling their await. Coalesce reload maintenance and give tasks cancellation/drain ownership; a `TaskTracker`/`JoinSet` may help if it serves that contract. Do not add locks around state already protected by user ownership.

ASYNC-005 in §3 records the reproduced metric cancellation defect; STEP-003 supplies its small independent fix and regression checks.

Cancellation/shutdown tests must include final durable writes and task completion, not just a cancelled token. See ASYNC-002/003/004 and PROTO-001 for concrete failures/risks.

## 9. Architecture and Refactoring Opportunities

No source file exceeds 800 lines. Highest counts: downstream root 733, service root 666, config 606; upstream registration 525, downstream registration 523, database types 516, history commands 513, ircutil 510 and webpush 505. Existing transport, listener, database, user and upstream module splits are already meaningful.

Clippy's logical-line counts identify more useful candidates than file size:

| Function / start | Logical lines reported | Proposed cohesive boundary |
| --- | ---: | --- |
| `upstream/users.rs:7 handle_message` | 325 | User/cache reply handlers per command family |
| `upstream/messages.rs:7 handle_message` | 296 | Message routing, batches, push scheduling |
| `upstream/channels.rs:7 handle_message` | 290 | JOIN/PART/KICK/topic/mode handlers |
| `upstream/registration.rs:103 handle_message` | 284 | CAP, SASL, registration numerics |
| `service/mod.rs:213 commands` | 226 | Declarative command registry separate from execution |
| `user/events.rs:134 handle_event` | 224 | Connection, account, push result and stop handlers |
| `downstream/cmds/history.rs:23 handle_chathistory` | 222 | Typed request, query plan, reply formatting |
| `config.rs:270 parse` | 189 | Directive-specific validation and construction |
| `downstream/cmds/webpush.rs:105 handle_webpush` | 164 | Subscription validation/admission/result lifecycle |
| `downstream/cmds/mod.rs:53 handle_message_registered` | 163 | Dispatch and validated context |

Other >100-line candidates: welcome, PRIVMSG, BOUNCER, READ, WHO/WHOIS, CLIENTCERT, WebSocket validation, user update and connection runner. These are review prompts, not a demand to split every match/table. Start with history and task ownership changes that directly remove repeated validation and optional-state assumptions. Avoid globally changing `Vec<Network>` layout, adding trait hierarchies, or boxing state without evidence; the [performance guide](performance-layout.md) explains the tradeoffs.

## 10. Testing and CI Assessment

The original baseline contained 152 library tests, two sojuctl tests, two listener integrations and 54 server integrations (27 scenarios run on both Tokio runtimes): **210 passing tests per GNU/musl run**. These tests are preserved; implementation adds four Rust regressions, bringing the current total to 214 per target. There are currently zero doctests. `--all-targets` additionally smoke-runs the custom reader harness. Existing coverage includes malformed PROXY/TLS/cert pinning, SASL chunks, credential redaction, stream cancellation, queues, multiline, migration/reopen/FTS, high-water IDs, user isolation, deferred service operations and dual-runtime behavior.

Shell syntax checks and Python AST parsing pass for the release/compatibility/benchmark scripts. These do **not** detect REL-001's missing workflow or prove script operational behavior.

Compatibility tooling is present for Go soju, ergo, IRCv3, plumbing and Chromium/gamja. It was reviewed but **not executed** in this run: it needs external checkouts/builds, Node/browser setup and multiple local services. No claim of transcript equivalence is made. Pin baseline revisions, retain a scheduled latest-upstream lane, and run those tools before completing protocol-affecting implementation steps.

Missing automation: pull-request/push triggers, lint/doc gates, tested-commit release dependency, advisory/license/banned-crate checks, parser fuzz corpus, appropriate native sanitizer jobs, controlled load/resource benchmarks. At audit time cargo audit/deny/machete/outdated/fuzz/Miri were absent. Audit/deny/machete have since run with actual results in §12; the other tools remain unavailable, not silently treated as passing. Use [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html), [Miri](https://github.com/rust-lang/miri) and [Tokio task shutdown guidance](https://tokio.rs/tokio/topics/shutdown) as implementation references; platform/native exclusions must be explicit.

## 11. Step-by-Step Implementation Plan

This is the authoritative checklist for the **next implementation execution**. P0 means immediate operational protection; P1 means correctness/resource/required gates; P2 means maintainability/extended verification. Completed checkboxes record verified audit or implementation work. Each step must update its related findings, record executed checks, and keep the checkout functional before proceeding. The user authorized implementation of phases 1–3 and a commit/push after each step; release publication and other remote mutations remain outside authorization.

### Phase 1 — Baseline and Critical Fixes

- [x] **STEP-001 (P0): Establish the audit baseline and live inventory.**
  - Findings: all. Objective/rationale: distinguish pre-existing failures from later regressions.
  - Files: inspect entire repository; create only `doc/audit_results.md` in this execution.
  - Instructions: record commit/toolchain, manifest/lock/features, test/lint/docs/build outcomes, release ordering and dependency sources; preserve existing changes.
  - Dependencies: none. Risks: socket/network sandbox limits and concurrent build timing.
  - Verification: §12 commands; `git diff --exit-code -- src Cargo.toml Cargo.lock .github scripts tests`.
  - Acceptance/status: **Completed (audit only)**; findings/plan delivered, implementation unchanged.

- [x] **STEP-002 (P0): Repair release dispatch and destructive-action preflight.**
  - Findings: REL-001, CI-001. Objective: make release helper behavior match actual workflow files before allowing replacement actions.
  - Files: `scripts/release-new.sh`, `scripts/release-replace.sh`, README; new hermetic release-script tests under `tests/` or `scripts/tests/`.
  - Instructions: dispatch the final existing release workflow; validate it/its inputs before mutation; keep lease/ref/version checks; stub git/gh and assert exact ordering. Initially use `release.yml`, then adapt to the tested reusable graph in STEP-026.
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
  - Instructions: classify §5 differences by selected target/parent constraint; read release notes for each breaking parent migration; evaluate Tokio full and optional HTTP/platform features; run machete or compiler-supported unused-dependency diagnostics. Remove only proven-unused requirements.
  - Dependencies: STEP-006. Risks: feature unification differences across tests/targets.
  - Verification: `cargo tree --target all -e features`, cargo audit/deny/machete, all-target tests and cross-target builds.
  - Acceptance/status: each remaining older line has a parent/compatibility reason; no unsupported patch overrides or untested removals; **Completed**.

- [x] **STEP-030 (P1 follow-up): Replace chrono with jiff.**
  - Findings: DEPS-002. Objective: remove the unmaintained timestamp dependency and make the advisory gate pass without suppressions.
  - Files: `Cargo.toml`, `Cargo.lock`, `src/xirc/mod.rs`, timestamp-bearing database/message-store/user/upstream/downstream modules, `src/logger.rs`, `src/server/configuration.rs`, and affected unit/integration fixtures.
  - Instructions: first capture chrono's accepted timestamp/date-range/leap-second behavior and local-time formatting in compatibility fixtures; choose explicit jiff timestamp/duration/time-zone APIs and review the current stable jiff release. Migrate types and conversions in cohesive groups, preserving canonical persisted TEXT values, chronological comparisons, nanosecond parsing and millisecond output. Preserve message IDs and schema 35; no schema bump is justified solely by a Rust time-library change. Remove chrono and its otherwise-unused transitives only after equivalence tests pass.
  - Dependencies: STEP-006–007. Initially added as planning only; the user subsequently authorized implementing the migration and the established commit/push workflow.
  - Risks: changed public Rust timestamp types, date ranges/leap-second acceptance, local timezone and DST behavior, duration overflow, history/read-marker boundaries, expiry and persisted timestamp sorting.
  - Verification: server-time tests for absent and 1–9 fractional digits, invalid dates/bounds and timezone policy; future-clock/read-marker/history tests; configured local-time logging tests; all-target check/Clippy and full tests/doc tests on GNU/musl; migration/reopen/foreign-key/FTS/cursor tests; optimized release/static-link/CLI checks; fresh cargo audit/deny/machete and `cargo tree --invert chrono` confirming removal.
  - Acceptance/status: chrono absent from the resolved graph, canonical database/wire timestamp fixtures unchanged except explicitly reviewed differences, required tests/releases pass, and no chrono advisory suppression added; **Completed**.

### Phase 4 — Compiler and Clippy

- [x] **STEP-008 (P1): Add compiler and core correctness lint policy.**
  - Findings: LINT-001. Objective: enforce high-value checks without an unreviewable all-at-once rewrite.
  - Files: `Cargo.toml`, affected production modules and focused test scopes.
  - Instructions: add prompt rust policy and Clippy all plus panic/todo/unimplemented/dbg/must-use/future checks; handle each diagnostic and ignored send/error by contract. Enable only a set whose violations are resolved in this step.
  - Dependencies: STEP-006. Risks: changing intentional shutdown error handling.
  - Verification: fmt/check/Clippy/tests; inspect lint exception reasons. Completed with 223 tests per GNU/musl target and strict all-target Clippy on both; reader smoke/doc tests pass. External Go/gamja compatibility remains not run; in-process protocol coverage passes. Error-mapping lint review is deferred with the remaining policy rather than changing credential-safe protocol errors in this step.
  - Acceptance/status: enabled policy passes on all targets and does not suppress production errors; **Completed**.

- [x] **STEP-009 (P1): Replace numeric/indexing hazards at external boundaries.**
  - Findings: NUM-001, LINT-001. Objective: make decoding/query/resource arithmetic explicit.
  - Files: `src/msgstore/{bare,db,memory}.rs`, `src/database/sqlite/messages.rs`, `src/rate.rs`, parser/PROXY sites flagged by Clippy.
  - Instructions: checked ID/length/limit conversion; document unlimited-limit policy; preserve intentional ZigZag bit patterns; checked sequence/rate configuration. Use slice patterns/get/split methods instead of repeated indexing after manual checks when clearer.
  - Dependencies: STEP-008. Risks: rejecting values previously accepted by library callers; preserve on-wire valid IDs.
  - Verification: boundary regressions, Go-compatible ID vectors, parser/database tests, selected cast/index lints.
  - Acceptance/status: no unchecked untrusted numeric conversion; each intentional cast/invariant justified; **Completed**.

- [x] **STEP-010 (P1): Propagate setup failures and model validated state.**
  - Findings: PANIC-001, LINT-001. Objective: remove meaningful panic paths, not substitute defaults.
  - Files: `src/auth.rs`, `webpush.rs`, `bin/soju.rs`, `server/configuration.rs`, downstream registration and upstream command/state APIs.
  - Instructions: Result-returning HTTP/TLS/signal/runtime construction; typed pending-command kinds; consolidate borrowed validated registration/upstream state; choose per-lock poison policy. Use narrow documented expectations for unreachable constant-size crypto failures only after review.
  - Dependencies: STEP-009. Risks: internal API propagation and state-machine behavior.
  - Verification: failure injection and invalid transition tests; SASL/cert/auth integration suite; unwrap/expect/panic lints.
  - Acceptance/status: fallible setup errors reach application boundary; no blanket exceptions or fabricated defaults; **Completed**.

- [x] **STEP-011 (P2): Complete pedantic policy and repair documentation.**
  - Findings: LINT-001, DOC-001, ARCH-001. Objective: enforce the final requested practical baseline.
  - Files: manifest lint tables, documented public APIs/tests, all rustdoc warning locations in DOC-001.
  - Instructions: work by diagnostic category; add error/panic contracts, simplify clones/combinators, repair Markdown. Finish long-function items after STEP-021–023 if necessary; do not mark the phase complete until final pedantic check passes. Permit test fixture expectations only with reasons.
  - Dependencies: STEP-010. Ordered dispatcher exceptions are now reviewed locally; larger ARCH-001 decomposition remains STEP-021–023 rather than being claimed fixed by this lint-policy step. Risks: cosmetic churn and overly broad exceptions.
  - Verification: complete baseline from prompt, strict rustdoc and full tests.
  - Acceptance/status: zero unapproved diagnostics, no blanket suppression; **Completed**.

### Phase 5 — Memory and Resource Safety

- [x] **STEP-012 (P1): Define and enforce aggregate memory-history budgets.**
  - Findings: MEM-001, NUM-001. Objective: bound retained history under target/message churn.
  - Files: `src/msgstore/memory.rs`, `src/msgstore/mod.rs`, config/docs if limits are configurable, store unit tests.
  - Instructions: specify target/byte accounting and cursor behavior before implementation; evict coherently; handle recreation without falsely treating old cursors as current; preserve network deletion.
  - Dependencies: STEP-009. Risks: intentional history truncation behavior; document it.
  - Verification: high-cardinality/large-message/cursor/eviction tests; byte/entry counters and sustained RSS experiment.
  - Acceptance/status: bounded counters independent of sender count; retained-history semantics explicitly tested; **Completed**.

- [x] **STEP-013 (P1): Bound accepted sessions including upgrades.**
  - Findings: MEM-002. Objective: cap sockets/tasks/handshake work globally.
  - Files: `src/listen.rs`, `src/listen/http.rs`, server/configuration/config docs, `tests/listeners.rs`.
  - Instructions: acquire admission without spawning unlimited waiters; transfer owned permit through TLS/PROXY/HTTP/WebSocket lifecycle; define operational default and overload response. Do not confuse forwarded identity with transport peer for limits.
  - Dependencies: STEP-008. Risks: connection rejection under legitimate load, upgrade lifetime errors.
  - Verification: incomplete-handshake/upgrade/error/close bursts; task/FD caps and release on all paths.
  - Acceptance/status: no permit leak or upgrade bypass; **Completed**.

- [x] **STEP-014 (P2): Assess receipt-cache growth and enumeration cost.**
  - Findings: PERF-001, MEM-001. Objective: avoid claiming a whole-user budget while cursors grow unchecked.
  - Files: `src/user/delivered.rs`, `src/user/events.rs`, receipt persistence tests/benchmark.
  - Instructions: benchmark cardinality; replace quadratic dedup if beneficial; specify bounded/lazy receipt cache without losing durable cursors or changing playback on reconnect. Keep disk cleanup as a separate documented retention decision.
  - Dependencies: STEP-012. Risks: replay loss/duplicates; ordering.
  - Verification: many-client benchmark, disconnect/restart receipt tests and bounded-cache reload tests.
  - Acceptance/status: measured behavior and explicit retention policy; no unverified eviction; **Completed**.

### Phase 6 — Async and Concurrency

- [x] **STEP-015 (P1): Repair connection writer ownership.**
  - Findings: ASYNC-004. Objective: free the writer/stream when its session is abandoned.
  - Files: `src/conn.rs`, reader/transport if needed, `src/conn/tests.rs`, downstream serve ownership.
  - Instructions: remove writer self-retention or add explicit owned session cancellation; preserve cloned handles/views and graceful queued writes. Clarify what dropping a reader means versus dropping the last session owner; do not use Arc strong-count races as a shutdown protocol.
  - Dependencies: STEP-010. Risks: premature close across clones/views or lost buffered output.
  - Verification: temporary probe converted to regression; idle/blocked/draining writers, serving-task abort, view tests.
  - Acceptance/status: abandoned sessions close and tasks finish within stated bounds; **Completed**.

- [x] **STEP-016 (P1): Add a bounded notification worker.**
  - Findings: ASYNC-001. Objective: constrain notification work before creating tasks.
  - Files: `src/upstream/messages.rs`, `src/user/webpush.rs`, `src/webpush.rs`, user/server lifecycle and metrics.
  - Instructions: bounded queue/worker concurrency; per-target coalescing and explicit overflow result/counter; integrate delayed read-marker checks; handle expiry deletion/cache invalidation. Treat notifications separately from mandatory IRC delivery.
  - Dependencies: STEP-015. Risks: altered push timing/order, superseded read markers, stale work after deletion.
  - Verification: mock slow endpoint flood, repeated MARKREAD, shutdown/deletion, subscription-expiry and registration-limit tests.
  - Acceptance/status: queue/task/request limits hold; no unbounded waiter tasks; **Completed**.

- [x] **STEP-017 (P1): Drain durable mention work before user completion.**
  - Findings: ASYNC-003. Objective: preserve mention state on orderly stop and retry transient failures.
  - Files: `src/upstream/messages.rs`, `src/user/events.rs`, `src/user/network.rs`, DB helpers and persistence tests.
  - Instructions: own flush scheduling, retain failed batches and merge timestamps monotonically; flush/join before completion; cancel work safely on network/user deletion; keep DB calls off executor threads as settled in STEP-019.
  - Dependencies: STEP-015; responsiveness verification after STEP-019. Risks: shutdown hangs, resurrection after deletion.
  - Verification: stop-before-delay/reopen regression, injected write failure/recovery, delete-during-flush.
  - Acceptance/status: successful orderly shutdown includes the latest durable mention; errors reported with defined policy; **Completed**.

- [ ] **STEP-018 (P1): Implement autonomous pending-command timeout.**
  - Findings: PROTO-001, PANIC-001. Objective: avoid permanently stuck serial queues and lost terminal replies.
  - Files: `src/upstream/commands.rs`, connection/timer events, `src/user/events.rs`, upstream server tests.
  - Instructions: typed active deadline event keyed by connection/command generation; complete downstream failures, advance or reconnect per late-response policy; stop timers on disconnect/deletion; preserve early-error correlation.
  - Dependencies: STEP-010, STEP-015. Risks: late replies routed to next request.
  - Verification: paused-time no-reply/no-third-request regression; late reply, label, disconnect and queue-cap tests.
  - Acceptance/status: requests terminate within policy without misrouting; **Not Started**.

- [ ] **STEP-019 (P1): Resolve current-thread database stalls.**
  - Findings: ASYNC-002. Objective: keep networking/timers responsive under persistence contention.
  - Files: `src/server.rs`, `src/bin/soju.rs`, `src/user/events.rs`, DB execution boundary and contention integration tests.
  - Instructions: measure the existing stall, then choose the smallest ownership-safe runtime/blocking-worker design; retain event serialization. Ensure started blocking work and shutdown have defined completion semantics.
  - Dependencies: STEP-008. Risks: worker handoff overhead and actor ordering.
  - Verification: current-thread/multi-thread contention, expiry/VACUUM, latency and normal-throughput comparison.
  - Acceptance/status: stated timer/connection latency bound met under controlled contention; no per-message unbounded spawning; **Not Started**.

- [ ] **STEP-020 (P1): Validate OAuth discovery trust and body limits.**
  - Findings: AUTH-001. Objective: bind provider metadata to the configured issuer.
  - Files: `src/auth.rs`, config/auth docs, mock HTTP auth tests.
  - Instructions: RFC issuer equality validation, secure endpoint/redirect policy and byte-bounded response decoding; retain intentional local testing support only through explicit policy. Preserve token/secret redaction.
  - Dependencies: STEP-010. Risks: provider configurations previously accepted become rejected; document required changes.
  - Verification: matching/mismatch discovery, HTTP/HTTPS/redirect/oversized/error/token cases, external auth integration tests.
  - Acceptance/status: mismatched issuer cannot authenticate, configured compatibility documented, secrets absent from logs; **Not Started**.

### Phase 7 — Architecture Refactoring

- [ ] **STEP-021 (P2): Separate history request parsing, query and response.**
  - Findings: ARCH-001, LINT-001. Objective: reduce the largest downstream validation/DB/reply function.
  - Files: `src/downstream/cmds/history.rs`, new cohesive history submodules only if helpful, history/read-marker tests.
  - Instructions: represent subcommands/bounds/limits in a typed request; parse once, execute store operation, format reply; preserve future-clock/tie/order/limit semantics and network scope. Keep READ changes separate if they do not share logic.
  - Dependencies: STEP-009, STEP-017. Risks: protocol boundary inclusivity/order changes.
  - Verification: BEFORE/AFTER/AROUND/BETWEEN/TARGETS, empty histories, equal timestamps, future clocks and both-runtime tests; compatibility transcript.
  - Acceptance/status: clean responsibilities, no changed valid transcript, no duplicate validation; **Not Started**.

- [ ] **STEP-022 (P2): Simplify protocol state handlers.**
  - Findings: ARCH-001, PANIC-001. Objective: separate command responsibilities and validated state access.
  - Files: `src/upstream/{users,channels,registration,messages,dispatch}.rs`, `src/downstream/{registration,welcome,caps}.rs`, `src/user/events.rs` as justified.
  - Instructions: one command-family extraction at a time with existing fixture/transcript preserved; borrow validated connection state rather than repeated macro unwraps; avoid new dynamic-dispatch layers. Do not fragment small cohesive helpers just to satisfy line counts.
  - Dependencies: STEP-015–019. Risks: event ordering, stale-connection handling, labeled-response/batch context.
  - Verification: targeted protocol tests after each extraction, full dual-runtime suite and pinned compatibility harness.
  - Acceptance/status: complex functions are cohesive and lint-reviewed; unchanged protocol/state behavior; **Not Started**.

- [ ] **STEP-023 (P2): Separate service registry and configuration concerns.**
  - Findings: ARCH-001, LINT-001. Objective: make large declarative registries and directive parsing easier to review.
  - Files: `src/service/mod.rs`, `src/config.rs`, command registry/parser modules as needed.
  - Instructions: move command declarations into a registry module; preserve admin/global/user constraints and help output; extract directive validation where coherent; replace `Option<&mut User>` assumptions through a validated dispatch contract where practical.
  - Dependencies: STEP-010, STEP-022. Risks: privilege/flag/default behavior regressions.
  - Verification: admin/non-admin/deferred/self-user/cross-user service scenarios; config roundtrips/errors; help transcript.
  - Acceptance/status: authorization invariant preserved; no arbitrary empty-user defaults; **Not Started**.

### Phase 8 — Testing and Fuzzing

- [ ] **STEP-024 (P1): Add parser properties, fuzz targets and migration stress cases.**
  - Findings: TEST-001, NUM-001. Objective: cover malformed inputs and persistence invariants beyond fixed fixtures.
  - Files: parser tests, database migration/storage tests, a separate `fuzz/` package and corpus.
  - Instructions: properties for IRC tags/parse-format, BARE/IDs, UTF-8 framing, multiline and durations; fuzz IRC/PROXY/BARE/scfg with explicit size budgets; add rollback/foreign-key/FTS integrity and genuinely concurrent file-backed initialization tests. Validate schemas 30–35 and older supported fixtures without modifying existing historical schemas.
  - Dependencies: relevant fixes and STEP-021–023. Risks: nondeterministic tests and unbounded generators.
  - Verification: deterministic property seeds, finite fuzz smoke runs, minimized crashing corpus, `PRAGMA integrity_check`, `foreign_key_check`, FTS integrity checks.
  - Acceptance/status: realistic adversarial coverage, no removed tests, documented corpus/seed reproduction; **Not Started**.

- [ ] **STEP-025 (P2): Establish supported native/safety analysis and soak coverage.**
  - Findings: TEST-001, MEM-001/002, ASYNC-001/004. Objective: examine native memory use and long-lived resources with appropriate tools.
  - Files: dedicated analysis script/workflow, pure parser test target if needed, resource benchmark harness.
  - Instructions: install pinned tooling in an appropriate environment; isolate pure Rust Miri targets when native FFI is unsupported; instrument SQLite/AWS-LC as well as Rust for meaningful sanitizers; use compatible allocator settings for sanitizer runs. Add repeated connect/disconnect, many-sender and slow-push soak measurements; avoid silently disabling tests.
  - Dependencies: STEP-012–020, STEP-024. Risks: nightly tooling differs from stable production, mimalloc/sanitizer interactions.
  - Verification: record Miri/ASan/LSan command, version, excluded paths and actual outcomes; monitor task/FD/byte counters and RSS over defined workload duration.
  - Acceptance/status: supported paths pass; unsupported paths have explicit blockers; resource plateau measured rather than inferred; **Not Started**.

### Phase 9 — CI Hardening

- [ ] **STEP-026 (P1): Require quality gates and tests before release.**
  - Findings: CI-001, REL-001, LINT-001, DOC-001. Objective: enforce tested-commit delivery.
  - Files: `.github/workflows/tests.yml`, `release.yml`, reusable workflow if needed, release helpers and README.
  - Instructions: add push/PR checks, workflow_call as appropriate; fmt/check/strict Clippy/test/strict doc; GNU/musl tests, locked release builds and static-link checks. Make archive build/publish depend on successful checks of the same SHA. Keep publishing manual and contents-write confined to publish job.
  - Dependencies: STEP-002, STEP-011 (including its architecture closure), STEP-024. Risks: workflow expression/permissions/target differences and increased runtime.
  - Verification: local YAML/workflow validation, dependency graph inspection, hermetic dispatch tests; later authorized CI runs must show test failure blocks release.
  - Acceptance/status: automatic quality gates exist; neither helper references an absent workflow; publish cannot bypass required tests; **Not Started**.

- [ ] **STEP-027 (P2): Add reproducible compatibility and scheduled maintenance lanes.**
  - Findings: CI-002, DEPS-001, TEST-001. Objective: detect upstream/security drift without destabilizing reproducible gates.
  - Files: workflows, `tests/compat/setup.sh`, compat README, dependency policy/configuration.
  - Instructions: pin source/action/tool revisions for required lane; log resolved commits/native/browser versions; schedule latest-upstream, cargo audit/deny/machete/freshness, bounded fuzz and supported sanitizer runs. Use documented license exceptions only after reviewing real results.
  - Dependencies: STEP-007, STEP-025, STEP-026. Risks: external-service availability; maintenance of pins/licenses.
  - Verification: pinned compatibility run on GNU; scheduled lane fails visibly on genuine issues; no continue-on-error masking required checks.
  - Acceptance/status: same commit/input pins reproducible, security tooling has recorded current results, scheduled failures observable; **Not Started**.

### Phase 10 — Final Verification

- [ ] **STEP-028 (P1): Run complete regression/deployment verification.**
  - Findings: all. Objective: demonstrate the final implementation preserves IRC/database/deployment behavior.
  - Files: full repository, `doc/audit_results.md` verification/progress sections.
  - Instructions: all prompt commands, both-runtime integrations on GNU/musl, full release archive builder, ELF interpreter/NEEDED assertions, checksum/archive CLI smoke tests, pinned IRCv3/plumbing/gamja/differential harness. Validate existing schema 35 database backup/reopen and migration fixtures; do not use a production DB as a test input without a safe copy.
  - Dependencies: STEP-026–027 and all P1 fixes; unresolved optional steps must be explicitly carried forward. Risks: external compatibility drift and native/target differences may expose failures missed by the host-only checks.
  - Verification: §12 full matrix rerun after implementation; `scripts/build-release.sh` and archive smoke checks without any remote release action.
  - Acceptance/status: required gates PASS, no unexplained protocol/schema differences, remaining findings assigned honest statuses; **Not Started**.

- [ ] **STEP-029 (P2): Measure performance and close the audit record.**
  - Findings: PERF-001, ARCH-001, resource/concurrency findings. Objective: substantiate performance preservation and production-readiness limits.
  - Files: benchmark/report documentation, `doc/audit_results.md`.
  - Instructions: build baseline/final optimized reader binaries, alternate same-workload measurements without simultaneous compilation; measure DB replay/target-list latency, single-CPU responsiveness, push saturation and repeated-connection resources. Record settings/hardware/sample ranges; update all findings only after implementation and verification.
  - Dependencies: STEP-028. Risks: noisy shared host and synthetic benchmarks overclaiming real throughput.
  - Verification: reader benchmark, existing connect-pages benchmark where practical, representative file-backed history and soak runs.
  - Acceptance/status: material regressions explained/resolved, no unsupported speedup/memory-safety claims, final limitations explicit; **Not Started**.

## 12. Verification Matrix

These results concern the **unchanged baseline**, not the proposed fixes. PASS means the command actually completed with success. NOT RUN is deliberately distinguished from BLOCKED.

| Check / executed command | Result | Evidence / limits |
| --- | --- | --- |
| `cargo fmt --all -- --check` | PASS | No changes needed |
| `cargo check --workspace --all-targets --all-features --locked` | PASS | Rust 1.100; GNU |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | PASS | Default policy only |
| Expanded Clippy trial in §6 | FAIL | 1,773 lib / 2,453 lib-test diagnostics; overlapping |
| `cargo test --workspace --all-features --locked` in sandbox | FAIL | 144 lib tests passed, eight socket tests denied by environment; integrations not reached |
| Same test command with approved local sockets | PASS | 210 tests; zero doctests |
| `cargo test --workspace --all-targets --all-features --locked` with sockets | PASS | 210 tests plus custom reader smoke run |
| `cargo doc --workspace --all-features --no-deps --locked` | PASS | 13 warnings; not warning-clean |
| `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked` | FAIL | Same documentation issues are errors |
| `cargo build --release --bins --locked` | PASS | GNU optimized binaries; no packaging/publication |
| `CC_x86_64_unknown_linux_musl=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo test --workspace --all-features --locked --target x86_64-unknown-linux-musl` with sockets | PASS | Same 210 tests; zero doctests |
| Same env, `cargo build --release --bins --locked --target x86_64-unknown-linux-musl` | PASS | Optimized musl binaries built successfully |
| musl ELF `readelf -lW` / `readelf -dW`, CLI smoke tests | PASS | Both musl binaries have no INTERP or NEEDED; GNU/musl `soju -version` exits 0 and `sojuctl -help` exits 2 with usage text |
| `scripts/build-release.sh`, extracted archives/checksums | NOT RUN | Build script reviewed; standalone release builds run separately |
| `cargo tree --locked --duplicates`, `cargo metadata --offline --locked --format-version 1` | PASS | Features/native inventory; active target only for duplicate tree |
| Live crates.io inventory | PASS | All locked registry names and all direct dependencies queried; no API errors |
| `cargo update --dry-run --offline` | PASS | No lock write; zero candidates in cached index, not a live freshness result |
| Release/compat shell `bash -n`; Python `ast.parse` | PASS | Syntax only; no remote release scripts executed |
| Temporary metric/connection/history probes | PASS | Successfully reproduce defects/retention; not evidence of fixes |
| Go/ergo/IRCv3/plumbing/gamja/differential compatibility | NOT RUN | External environment not provisioned in this run |
| `cargo audit`, `cargo deny`, `cargo machete`, `cargo outdated` | BLOCKED | Commands absent in `cargo --list`; no tool installation performed |
| Miri | BLOCKED | No rustup/Miri; native dependencies require a suitable isolated target |
| ASan/LSan | NOT RUN | No supported sanitizer/native instrumentation configured |
| Parser fuzzing | BLOCKED | No cargo-fuzz or targets/corpus yet |
| Long-running memory/FD/task soak | NOT RUN | Finite probes and bounded-queue tests do not replace a soak |
| Before/after performance | NOT RUN | Audit-only: no changed binary to compare |

### Implementation verification for phases 1–3

These rows supersede the original tooling-availability/freshness blockers;
they do not imply completion of later hardening phases.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| STEP-002 release script tests | PASS | Six unittest methods with git/gh stubs; no release actions executed |
| STEP-002 bash syntax and GNU/musl release builds | PASS | All three shell scripts parse; both native builds succeed |
| STEP-003 fmt/check/Clippy, full tests and doc tests | PASS | 213 tests and reader smoke; rustdoc builds with 13 existing warnings |
| STEP-004 declared Rust 1.100 floor | PASS | Installed stable 1.100; 213 GNU/musl tests and required compiler checks |
| STEP-005 compatible updates | PASS | 213 GNU/musl tests, releases, static linkage and CLI smoke |
| STEP-006 rusqlite migration | PASS | 214 GNU/musl tests, file-backed schema-30→35 reopen/FTS/FK/cursor/ID regression, native releases and static-link/CLI checks |
| STEP-007 reduced features | PASS | 214 GNU/musl tests, fmt/check/Clippy, reader smoke/doc tests, both releases and static-link/CLI checks |
| STEP-008 core lint policy | PASS | fmt/check, strict all-target Clippy on GNU/musl, 223 tests per target, reader smoke/doc tests; four new operational boundary regressions; rustdoc retains 13 existing warnings |
| STEP-030 Jiff migration | PASS | fmt/check/strict Clippy; 219 tests per GNU/musl target plus reader smoke/doc tests; legacy TEXT/schema/FTS/cursor/ID preservation; timezone/DST and temporal boundary fixtures; both optimized releases/static-link/CLI; rustdoc with 13 existing warnings |
| Live registry dry run after upgrades | PASS | `cargo update --dry-run`: zero compatible updates |
| Current advisory scan | PASS | STEP-030: cargo-audit 0.23.0 reports zero known vulnerabilities and no advisory warnings |
| Full dependency policy | PASS | STEP-030: cargo-deny 0.21.0 accepts advisories, bans, licenses and sources; existing syn/untrusted duplicate warnings remain; no advisory ignored |
| License/source/duplicate policy subset | PASS | `cargo deny --all-features --locked check bans licenses sources`; parent-owned duplicates remain warnings |
| Unused direct dependency analysis | PASS | cargo-machete 0.10.1 with metadata finds no unused direct requirements |

Tool installation and registry-fetch failures under the restricted sandbox
were retried with approved access. Tools live under `/tmp/soju-audit-tools`;
registry/analysis logs use `/tmp/soju-step-<step>-*.log` and advisory JSON files.
The STEP-030 dependency graph removes chrono; the advisory gate now passes without exceptions.

Temporary logs: `/tmp/soju-audit-test.log`, `soju-audit-test-unrestricted.log`, `soju-audit-all-targets.log`, `soju-audit-musl.log`, `soju-audit-doc.log`, `soju-audit-strict-doc.log`, `soju-audit-strict-clippy.log`, `soju-audit-release.log`, `soju-audit-musl-release.log`, `soju-audit-update-dry-run.log`. Paths after the first share `/tmp/`. Temporary files are convenience evidence, not durable repository artifacts; key outcomes are recorded here.

## 13. Compatibility and Performance Risks

- **Database compatibility:** Keep schema 35, high-water ID allocation, clustered history/FTS and lossily decoded upstream text intact. README's introductory “same database” claim is qualified later: older upstream/soju-rs cannot open an upgraded schema 35 database. Preserve/document this existing one-way upgrade; no new migration is proposed just for modernization. Back up before migration validation.
- **Protocol compatibility:** Preserve IRC framing/redaction, SASL failures, labeled-response ownership, batch/multiline semantics, dual-runtime ordering, multiplexing and future server clocks. Bounded history/push/connection policies introduce explicit operational behavior and require documentation and transcript tests.
- **Scheduling:** Moving DB work off the single runtime thread may add handoffs. Admission limits may reject legitimate bursts. Choose limits from measurements and test slow-client fairness; do not create unlimited permit-waiting tasks.
- **Crypto/native:** Keep certificate pinning leaf-only and handshake signature validation; optional client certificates rely on later fingerprint authentication, not CA trust. Do not weaken those intentional trust boundaries during dependency migration. GNU/musl builds are required but do not establish arbitrary-platform support.
- **Measurement:** Existing reader/performance documents are historical measurements, not results rerun here. This audit made no hot-path change and claims no speedup. New benchmarks must separate parsing, DB lock latency, notification work and whole-server traffic.
- **Rollback:** Dependency changes should be isolated/reviewed; schema remains unchanged. Cursor/eviction and new issuer policy changes need explicit compatibility notes. Keep the last verified working state if a major migration cannot be validated.

## 14. Remaining Issues and Blockers

**Remaining high-priority work:** CI-001 test-before-release enforcement, MEM-001 aggregate history growth and ASYNC-001 unbounded task admission. REL-001 is fixed in STEP-002. No critical defect was confirmed.

**Remaining medium/low work:** ASYNC-004 abandoned-writer retention, PROTO-001 enqueue-only expiry, remaining architecture/lifecycle hardening. Compiler/pedantic policy, strict rustdoc and numeric-boundary work are now enforced by STEP-009–011. ASYNC-005 metric cancellation, RUST-001 compiler policy and DEPS-001 version freshness are resolved by their completed steps. DEPS-002 is resolved by the verified STEP-030 Jiff migration. ASYNC-003's lifecycle gap is evident in ownership/shutdown code; the full file-backed loss scenario must become a regression before claiming a completed fix.

**Potential risks needing targeted measurement/validation:** accepted-session saturation, single-runtime DB stalls under contention, receipt-cache cardinality, provider metadata trust configurations, failure/poison paths, native/advisory exposure. Do not label these all exploitable vulnerabilities.

**Optional improvements:** cohesive function/module extraction, measured receipt deduplication, feature pruning and reproducibility refinements. None justify speculative architecture/allocator rewrites.

**Current verification limits:** cargo-audit/deny/machete are installed for the implementation run and their results are recorded below. STEP-030 removes chrono and the full cargo-deny policy passes without advisory ignores; duplicate-major warnings remain visible. cargo-outdated/Miri/fuzz tooling and native sanitizer coverage remain unavailable/not configured; external compatibility and soak checks remain not run. A refreshed Cargo index now reports no compatible updates. No approval rejection prevented an action; registry/socket checks used approved escalation.

**Execution boundary:** the original audit deferred implementation under [audit_prompt.md §17.5](audit_prompt.md#175-execution-order--mandatory). The user subsequently authorized phases 1–3, STEP-008, the STEP-030 migration, and commit/push after each step. Other later phases remain outside the current request. Mark findings Completed only after implementation and verification.

## 15. Implementation Progress Log

| Date / phase | Work actually completed | Verification / remaining work |
| --- | --- | --- |
| 2026-11-17 / audit scope | Read full prompt; applied final audit-only instruction; inventoried package/source/SQL/tests/scripts/docs/workflows | No implementation authorized for this run |
| 2026-11-17 / baseline | fmt/check/default Clippy, GNU/musl tests, doc/strict-doc, strict-Clippy trial, release builds | Results in §12; environmental socket failures rerun successfully |
| 2026-11-17 / research | Verified official Rust 1.100 release; queried all direct/locked crate names; read rusqlite 0.43 release notes | Candidate migrations identified, none applied |
| 2026-11-17 / focused probes | Cancelled metric guard, dropped connection ownership, 10,000 history targets | Defects/retention reproduced; tests not added in audit-only phase |
| 2026-11-17 / planning | Created findings, 10 phases, 29 actionable steps with dependencies/risks/checks/acceptance criteria | STEP-001 completed; STEP-002–029 not started |
| 2026-11-17 / STEP-002 | Release helpers now preflight the active remote release.yml against the local definition before mutation; six hermetic regression tests and bash syntax checks pass. Ruff and ShellCheck are unavailable. CI-001 remains open for phase 9. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-003 | Downstream and upstream counts now use one RAII guard; regressions cover completion, operational error, cancellation and unwind. fmt, check, default Clippy, 213 tests, reader smoke and doc tests pass. Rustdoc builds with the 13 pre-existing warnings. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-004 | Declared rust-version 1.100 with Edition 2024 unchanged; README and AGENTS now document the floor and distro/rustup support. Installed stable 1.100 passes fmt, check, Clippy and 213 tests on both GNU and musl, plus reader smoke and doc tests. CI Cargo commands enforce the manifest floor. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-005 | Applied seven compatible stable upgrades from the refreshed registry, including rand 0.10.4. fmt/check/Clippy, 213 GNU and musl tests, reader smoke/doc tests, both release builds and musl static linkage/CLI smoke pass. cargo-audit finds no known vulnerabilities and reports the unsuppressed chrono maintenance warning; graph downloads required network escalation. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-006 | Migrated rusqlite 0.42 to 0.43 without changing libsqlite3-sys 0.40.0, SQLite 3.54.1 or schema 35. New file-backed migration/reopen/ID/cursor/foreign-key/FTS regression passes. fmt/check/Clippy, 214 GNU/musl tests, reader smoke/doc tests, release/static-link/CLI checks pass. Audit finds no known vulnerabilities; deny preview fails on unsuppressed DEPS-002. Added the user-requested jiff migration as planning-only STEP-030. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-007 | Completed version/feature assessment: all 34 retained older versions have documented parent constraints. Removed unused Tokio process/stdio and SQLite WASM features while preserving native cache/locking; removed two lockfile packages. fmt/check/Clippy, 214 GNU/musl tests, reader smoke/doc tests and releases/static-link/CLI pass. Machete and license/source checks pass; full deny FAILS on unsuppressed DEPS-002. Jiff migration remains planning-only STEP-030. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-030 | Replaced chrono with Jiff 1.0.0 across timestamp APIs, persistence, elapsed intervals and local logs. Preserved millisecond TEXT/schema 35, nanosecond input, pre-epoch Unix topic rounding, history/future clocks and IDs/cursors/FTS. Documented the intentional leap-second normalization and narrower date range. Added compatibility, timezone/DST, storage and duration-overflow regressions. fmt/check/strict Clippy, 219 tests on each GNU/musl target, reader smoke/doc tests, both optimized releases/static-link/CLI checks pass. Rustdoc builds with 13 existing warnings. Fresh audit has no vulnerabilities or advisory warnings; full deny and machete pass without ignores. Registry dry run finds no compatible updates. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-008 | Enforced the core Rust compiler policy and Clippy all/panic/todo/unimplemented/dbg/must-use/future checks plus allow-attribute rejection. Resolved explicit production panic sites, handled operational results and documented narrow cancellation/cleanup/test exceptions. Failed writer enqueue now rolls back queue accounting and closes the physical connection; transport shutdown errors are preserved. Added four regressions for failed sends, close errors, unsupported pending commands and duplicate nickname retry timers. fmt/check, strict GNU/musl all-target Clippy, 223 tests per target, reader smoke and doc tests pass. Rustdoc builds with the 13 existing warnings. LINT-001 remains In Progress for STEP-009–011; no dependency, schema or workflow change. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-009 | Enabled checked numeric-cast policy; replaced lossy BARE/SQLite/message-ID/source-address conversions, guarded memory sequence exhaustion, and validated rate configuration. Preserved Go ID vectors and documented explicit unlimited SQLite limits. PROXY input uses checked slices and patterns with focused indexing enforcement. fmt/check/strict Clippy and 227 GNU/musl tests plus reader smoke/doc tests pass. 32-bit support is not claimed; remaining internal indexing policy is staged for STEP-011. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-010 | Made HTTP/TLS/Web Push setup fallible, propagated runtime/signal errors, typed pending-command kinds, and checked registration/upstream/service state. Removed operational unwrap/expect paths and documented fail-closed actor/auth locking separately from SQLite rollback-based recovery. Added six setup/state/poison regressions; limited invariant/fixture expectations to reasoned local scopes. Enabled unwrap/expect lints. fmt/check/strict Clippy and 233 GNU/musl tests plus reader smoke/doc tests pass. Rustdoc retains the 13 existing warnings for STEP-011. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-011 | Completed the prompt compiler/Clippy baseline, including pedantic, indexing/string-slice and error-mapping checks. Added API error/panic contracts, repaired rustdoc links/grammar, reused clones and preserved fixed protocol messages while retaining safe error causes. Reviewed statement-level bounds contracts and function-specific ordered-dispatch exceptions; ARCH-001 extraction remains a separate Phase-7 task and is not claimed resolved. No crate/module-wide lint suppression was added. fmt/check and strict GNU/musl all-target Clippy pass; all 233 tests per target plus reader smoke/doc tests pass. Strict rustdoc now passes with zero warnings. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-012 | Bound per-user memory history to 1,024 targets, 16,384 messages and 16 MiB accounted storage with coherent eviction and incarnation-scoped cursors. Churn/cursor/accounting tests and full GNU/musl checks (235 tests each), strict Clippy/rustdoc pass. Finite 800k-target/40k-large-record experiment plateaus at 1,024 targets / 1,010 large records; RSS measurements and accounting limits documented in resource-limits.md. Receipt-cache portion of MEM-001 remains for STEP-014. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-013 | Global accepted-session admission defaults to 256, is reloadable, rejects before spawning, and follows physical streams through TLS/PROXY/HTTP/WebSocket upgrades. Cancellation closes session writers. Concurrent admission, 300 overload attempts, close/error/upgrade/reload/admin/shutdown/abort regressions pass; full GNU/musl checks (240 tests each), strict Clippy/rustdoc and documentation links pass. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-014 | Bound persistent receipt dirty caches per user to 4,096 entries/4 MiB with write-through pressure handling, lazy 128-row reconnect pages, upsert-only partial flushes and failure-safe accounting. Preserve memory-only receipt lifetime and explicitly exclude it from whole-user memory claims. Seven-sample many-client benchmark documents small-client overhead and removes quadratic enumeration (4,096 clients: 393.526 ms to 3.052 ms). Shared pressure, 10k-row file reload, aliases/legacy bytes, failure/atomic initialization and reconnect/restart tests pass. Full GNU/musl checks (246 tests each), strict Clippy/rustdoc, cargo audit/deny/machete pass; schema 35 and durable rows preserved. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-015 | Remove writer-owned senders/session owners. Explicit shared ownership keeps clones/views valid, cancels abandonment and rate/write waits, and preserves requested graceful draining. Attachment guards remove actor clones and persist receipts after serving-task abort. Idle/blocked/rate/drain/view and both-runtime abort regressions pass, including the ten-second abandoned-writer close bound. Full GNU/musl checks (254 tests each), strict Clippy/rustdoc and documentation checks pass. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-016 | One bounded/coalescing worker per user admits at most 128 pending conversations/1 MiB and one active job; sender-wide semaphore limits encryption/HTTP to 32 requests. Integrate delayed read checks, MARKREAD, overflow metrics, joined stop, network cancellation and registration-test stop tokens. Recheck scoped subscription snapshots and conditionally delete expiry to protect replacement IDs/keys; invalidate subscription caches. Flood/timing/byte/cancel/expiry/owner/global-cap/25-subscription/four-test-slot regressions pass. Full GNU/musl checks (264 tests each), strict Clippy/rustdoc and documentation checks pass; stale Unix-socket fixture now waits for actual refusal. | Completed; commit/push follows verification |
| 2026-11-17 / STEP-017 | Replace detached mention flushes with actor-owned maps/deadlines and joined blocking batch transactions. Clear only after commit, retain/merge failures and retry autonomously; stop flushes all batches with three attempts and explicit failure metrics/logs. Network updates transfer state, deletion is serialized and foreign keys prevent stale resurrection. Stop-before-delay/reopen, rollback/recovery, monotonic/delete-during-flush and permanent-error tests pass. Full GNU/musl checks (269 tests each), strict Clippy/rustdoc and documentation checks pass. STEP-019 remains for handler responsiveness. | Completed; commit/push follows verification |

On every subsequent implementation phase: append changes and actual command results, update finding statuses, record deviations, and retain failed-check evidence. Final verification must be rerun against the changed code; baseline passes are not transferable.

## 16. Final Recommendations

The original phases 1–3 now establish release preflight, cancellation-safe metric accounting, the compiler floor and reviewed dependency upgrades. STEP-008 now enforces the core compiler/Clippy policy. Next, finish the staged numeric, unwrap/expect, pedantic and error-mapping policy and address resource/lifecycle hazards with focused regressions before broad refactoring. The completed STEP-030 Jiff migration resolves the maintenance advisory and makes the full dependency policy pass. Stage strict lint enforcement by meaningful category. Close the audit only after same-commit CI/release gating, GNU/musl builds, protocol/database compatibility and supported security/resource checks have recorded actual results.

This document preserves the audit and its implementation checklist. Implementation outcomes are recorded by finding status and in §15; baseline measurements describe the original audit.
