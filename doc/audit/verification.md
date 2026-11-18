# Audit verification evidence

[Audit summary](../audit_results.md) · [Record index](README.md)

## 12. Verification Matrix

These results concern the **unchanged baseline**, not the proposed fixes. PASS means the command actually completed with success. NOT RUN is deliberately distinguished from BLOCKED.

| Check / executed command | Result | Evidence / limits |
| --- | --- | --- |
| `cargo fmt --all -- --check` | PASS | No changes needed |
| `cargo check --workspace --all-targets --all-features --locked` | PASS | Rust 1.100; GNU |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | PASS | Default policy only |
| Expanded Clippy trial in [§6](baseline.md#6-compiler-and-clippy-hardening) | FAIL | 1,773 lib / 2,453 lib-test diagnostics; overlapping |
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
| Parser fuzzing | BLOCKED | No cargo-fuzz or targets/corpus yet |
| Long-running memory/FD/task soak | NOT RUN | Finite probes and bounded-queue tests do not replace a soak |
| Before/after performance | NOT RUN | Audit-only: no changed binary to compare |

### Earlier implementation verification

These rows supersede the original tooling-availability/freshness blockers;
later hardening verification is recorded separately below.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| STEP-002 release script tests | PASS | Six unittest methods with git/gh stubs; no release actions executed |
| STEP-002 bash syntax and GNU/musl release builds | PASS | All three shell scripts parse; both native builds succeed |
| STEP-003 fmt/check/Clippy, full tests and doc tests | PASS | 213 tests and reader smoke; rustdoc builds with 13 existing warnings |
| STEP-004 declared Rust 1.100 floor | PASS | Installed stable 1.100; 213 GNU/musl tests and required compiler checks |
| STEP-005 compatible updates | PASS | 213 GNU/musl tests, releases, static linkage and CLI smoke |
| STEP-006 rusqlite migration | PASS | 214 GNU/musl tests, file-backed schema-30→35 reopen/FTS/FK/cursor/ID regression, native releases and static-link/CLI checks |
| STEP-007 reduced features | PASS | 214 GNU/musl tests, fmt/check/Clippy, reader smoke/doc tests, both releases and static-link/CLI checks |
| STEP-009 core lint policy | PASS | fmt/check, strict all-target Clippy on GNU/musl, 223 tests per target, reader smoke/doc tests; four new operational boundary regressions; rustdoc retains 13 existing warnings |
| STEP-008 Jiff migration | PASS | fmt/check/strict Clippy; 219 tests per GNU/musl target plus reader smoke/doc tests; legacy TEXT/schema/FTS/cursor/ID preservation; timezone/DST and temporal boundary fixtures; both optimized releases/static-link/CLI; rustdoc with 13 existing warnings |
| Live registry dry run after upgrades | PASS | `cargo update --dry-run`: zero compatible updates |
| Current advisory scan | PASS | STEP-008: cargo-audit 0.23.0 reports zero known vulnerabilities and no advisory warnings |
| Full dependency policy | PASS | STEP-008: cargo-deny 0.21.0 accepts advisories, bans, licenses and sources; existing syn/untrusted duplicate warnings remain; no advisory ignored |
| License/source/duplicate policy subset | PASS | `cargo deny --all-features --locked check bans licenses sources`; parent-owned duplicates remain warnings |
| Unused direct dependency analysis | PASS | cargo-machete 0.10.1 with metadata finds no unused direct requirements |

Tool installation and registry-fetch failures under the restricted sandbox
were retried with approved access. Tools live under `/tmp/soju-audit-tools`;
registry/analysis logs use `/tmp/soju-step-<step>-*.log` and advisory JSON files.
The STEP-008 dependency graph removes chrono; the advisory gate now passes without exceptions.

### Final verification through Phase 6

Per user direction, STEP-019–021 implementations preceded the combined final
checks. The implementation commits kept those steps In Progress until these
checks and measurements completed. Earlier failed checks are recorded as
resolved failures rather than counted as passes.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| Initial combined checks | FAIL, resolved | Added the tokio-util task-tracking lockfile edge without changing versions; fixed strict-lint diagnostics and the timeout fixture's capability setup before the final passing runs |
| Formatting and locked workspace/all-target/all-feature check | PASS | Final source, plus the benchmark-target smoke rerun after extending its dispatch comparison |
| Strict Clippy | PASS | All targets/features on GNU and musl; no broad lint suppression |
| Full GNU/musl suites | PASS | 282 tests per target; both Tokio runtimes; custom reader harness and zero-failure doc tests |
| Strict rustdoc | PASS | `RUSTDOCFLAGS=-D warnings`, zero warnings |
| Optimized GNU/musl binaries | PASS | Both binaries; musl has no ELF interpreter or NEEDED entries; version/help CLI smoke succeeds |
| Advisory, license/source/ban and unused-dependency checks | PASS | cargo audit/deny/machete; duplicate-major warnings remain visible; no new advisory ignore |
| Controlled resource/ownership/persistence/auth regressions | PASS | History/receipt churn, transport admission, writer teardown, notification saturation, mention draining, command deadlines, SQLite contention and OAuth trust/redaction |
| Actor contention and normal parsed-IRC comparisons | PASS | Recorded in [resource limits](resource-limits.md#actor-boundary-measurements), including measured handoff/throughput overhead |
| External Go/ergo/IRCv3/plumbing/gamja comparison | NOT RUN | External environment remains unprovisioned; no transcript-equivalence claim |
| Sustained resource soak and parser fuzz corpus | NOT RUN | Remain future STEP-025–026 work; finite regressions and benchmarks are not a long soak |

Final check logs use `/tmp/soju-step-phase6-final-check-*.log` and
`/tmp/soju-phase6-final-*.log`. Benchmarks use fixed finite workload counts;
their timing values are specific to the measured host and runtime choices.

Temporary logs: `/tmp/soju-audit-test.log`, `soju-audit-test-unrestricted.log`, `soju-audit-all-targets.log`, `soju-audit-musl.log`, `soju-audit-doc.log`, `soju-audit-strict-doc.log`, `soju-audit-strict-clippy.log`, `soju-audit-release.log`, `soju-audit-musl-release.log`, `soju-audit-update-dry-run.log`. Paths after the first share `/tmp/`. Temporary files are convenience evidence, not durable repository artifacts; key outcomes are recorded here.

### Final verification through Phase 7

All three implementations preceded test/lint/gate execution, per user direction.
The user allowed compilation-only checks between steps. The final checks below
cover the final source and preserve all earlier regressions.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| Formatting and locked workspace/all-target/all-feature check | PASS | `cargo fmt --all -- --check` and `cargo check --workspace --all-targets --all-features --locked` |
| Strict GNU/musl Clippy | PASS | All targets/features, `-D warnings`; removed ten existing long-function exceptions from refactored functions; no new exceptions |
| Full GNU/musl test suites | PASS | 287 tests per target: 213 library, one server binary, two admin binary, five listeners, 66 server tests; custom reader smoke and doc tests also pass |
| Strict rustdoc and GNU binary build | PASS | Zero rustdoc warnings; both debug binaries built with all features for the protocol comparisons |
| Same new assertions on original Rust | PASS | Original `1d8f5815e3857e1102ea2be0caff353c243a3587` passes the new two-runtime history tie/shortcut scenario, help snapshots and config precedence/default/alias checks |
| Pinned Go-soju/ergo differential | FAIL, retained | All twelve selected scenarios report transcript differences, including capability sets/CAP splitting, Rust service extensions and deferred-operation timing; no clean Go-equivalence claim |
| Original/final Rust differential | PASS with fixture corrections | Ten of twelve scenarios pass directly. Raw history TARGETS equal-time selection and deferred service timing differ; the same two scenarios pass with distinct message times and explicit completion barriers. Parameter-free channel-mode flag sets are sorted in this temporary comparison, in addition to the existing volatile-field/map-order normalization |
| Markdown links/anchors and diff whitespace | PASS | Final documentation checked after recording results |
| IRCv3/plumbing/gamja and sustained soaks | NOT RUN | Remain separate verification work; no browser/deployment/soak result inferred from these finite scenarios |

Pinned inputs: Go soju `de8690c06fd31e2d34bc84926a6620c5ab2796f0`,
ergo `47b63c9bb58012311274c26c8b950201ec40364e`; original Rust
`1d8f5815e3857e1102ea2be0caff353c243a3587`. The selected scenarios are
registration/bouncer, registration errors, channels, history, read markers,
labeled replies, service commands, capability negotiation, nick/realname,
away, invite/WHO cache and broadcast/user-run.

Initial verification failures were resolved before these passing local gates:
one extracted unused routing parameter, strict-lint diagnostics and a malformed
configuration test fixture. Sandbox socket denials were retried with approved
local-socket access. A shared-target baseline run exposed stale GNU test artifacts;
package artifacts were cleared and final GNU/musl gates rerun, explicitly checking
the new registry test and 213-library-test counts. The later baseline binaries
use a separate explicit GNU target directory. Original Go and Rust comparison
failures are retained rather than counted as passing raw runs. Fixture corrections
live in the temporary comparison driver; the repository harness is unchanged.

Local logs: `/tmp/soju-step-phase7-final-check-*.log`,
`/tmp/soju-phase7-{musl-clippy,rustdoc,bins}.log`. Original assertion logs use
`/tmp/soju-phase7-baseline-*.log`; Go differences are in
`/tmp/soju-phase7-compat/difflatest.txt`, and Rust comparison reports are
`/tmp/soju-phase7-rust-diff.log` and `/tmp/soju-phase7-rust-diff-stable.log`.
Temporary services were stopped after both comparisons. Key outcomes and pins
are recorded here because temporary logs are not durable repository artifacts.

### Final verification through Phase 8

Both implementations preceded tests/lint/gates per user direction. The fuzz
package is under `tests/fuzz/` as requested. [Resource stress results](resource-soaks.md)
record the fixed seeds, workload sizes, wall-clock durations, resource samples,
actual failures/fixes and limitations.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| fmt, locked check and strict GNU/musl Clippy | PASS | All workspace targets/features; standalone fuzz package also passes formatting/check/Clippy |
| GNU/musl suites | PASS | 298 tests per target plus reader smoke/doc tests; opt-in timed tests run separately |
| Strict rustdoc | PASS | Zero warnings |
| Parser properties | PASS | Fixed seeds and semantic models for tags/messages, BARE/IDs, UTF-8, multiline and durations |
| Historical schemas and integrity | PASS | Schemas 30–35, original older fixture, rollback, FTS integrity and foreign-key checks |
| Concurrent initializers | PASS after fix | Original genuine race failed at WAL setup; bounded WAL busy retry fixes it, including twenty repeated eight-connection tests |
| Mutation campaigns | PASS after harness correction | Four targets × two seeds × 10,000 mutations; reduced four-byte malformed-command counterexample retained; no coverage-guided claim |
| Timed resources | PASS | Two 180-second workload models plus five-second drain; additional CAP exchange run exercises real writer teardown; precise counter/RSS ranges in resource guide |
| Python runner | PASS | Syntax plus actual controlled workload execution; Ruff unavailable |
| Go equivalence, browser/deployment and multi-hour soaks | NOT ESTABLISHED | Phase 7's raw Go mismatches remain; no clean Go/browser/deployment result inferred from local Phase 8 tests |

Earlier failures are retained as resolved failures: typed SQLite fixture value,
lint diagnostics, incorrect empty/trailing-newline multiline expectation,
permissive-command canonicalization assumption, WAL initializer race, and a
diagnostic context that initially obscured the existing newer-schema error.
Historical schema text remains unchanged; new tests and fixture construction
use the original migrations and FTS upgrade hook.


### Final verification through Phase 9

STEP-027–028 are verified with the explicit limitations in the
[final assessment](final-verification.md), which records pins, settings, sample
ranges, corrected failures, raw transcript differences and reproduction details.

| Check | Actual result | Evidence / limit |
| --- | --- | --- |
| fmt, locked all-target/all-feature check, strict GNU/musl Clippy | PASS | Final production source; no Rust implementation change in this phase |
| GNU/musl tests, reader smoke and doc tests | PASS | 298 tests per target; both Tokio runtimes; two opt-in soaks run separately |
| Strict rustdoc and standalone fuzz gates | PASS | Zero rustdoc warnings; fuzz fmt/check/Clippy plus 80,000 mutations |
| GNU/musl archive builder and checksums/extracted CLI/ELF | PASS | No musl INTERP/NEEDED; version/help exit contracts checked |
| Restored schema-35 archive deployment | PASS | Both extracted targets, admin query, orderly shutdown, 100k IDs/high-water/FK/FTS integrity |
| IRCv3 and Rust plumbing | PASS | 32/32 and 20/20 after resolving environment-startup mistakes |
| Local Gamja browser scenarios | PASS | 4/4 against Rust and 4/4 against pinned Go; tracked source from `../gamja` |
| Local Senpai suite and real-bouncer integration | PASS | `../senpai`, GNU release supplied explicitly; verbose integration passes in 8.36 seconds |
| Pinned Go/Rust differential | FAIL, retained | Same twelve scenarios differ in capabilities, CAP splitting, service output and timing; corrected service/broadcast fixtures also retain Go differences |
| Original/final Rust differential | PASS with fixture correction | Eleven scenarios pass directly; service scenario passes with explicit completion replies; corrected broadcast also passes |
| Reader, actor, receipt, file-backed SQL and connect-pages measurements | EXECUTED | Five alternating reader pairs; one CPU; ranges and handoff costs in final assessment; SQL timings are not server-latency claims |
| Final connection/history and slow-push soaks | PASS | Each runs 180 seconds plus five-second drain; retained counters bounded and cleanup verified |
| Final cargo-audit/deny/machete | NOT RUN | Commands absent in this session; earlier scans remain historical, no fresh advisory claim |
| Multi-hour/continuous fuzz/complete production deployment coverage | NOT ESTABLISHED | Explicit finite-check limits; CI findings remain unresolved |

The compatibility harness now waits for deferred service completion before
issuing dependent commands. Both changed scenarios were functionally rerun
against original/final Rust and Go/Rust. Python syntax passes; Ruff is unavailable.
Markdown links/anchors and whitespace are checked before commit. Logs and
measurements use `/tmp/soju-phase9-*`; key results are retained in the final guide.
