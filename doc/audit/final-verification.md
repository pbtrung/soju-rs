# Final verification and performance assessment

[Audit summary](../audit_results.md) · [Checklist](implementation-plan.md) ·
[Verification matrix](verification.md)

Phase 9, STEP-027–028, measured on 2026-11-18. Production Rust code, dependencies
and schema are unchanged from `5682f8d`; the compatibility fixture now waits for
deferred service completion. Completion records the checks and limits below,
not exact Go equivalence or certification of every deployment.

## Inputs and local gates

Host: Linux x86_64, Intel Xeon E-2176M at 2.70 GHz, 12 logical CPUs, 64 GB RAM;
Rust 1.100.0, system SQLite 3.54.1. Optimized reader measurements were pinned to
CPU 0, alternated after compilation and resource soaks completed. Other host
activity and local compatibility servers were not excluded.

| Input | Revision |
| --- | --- |
| Original Rust audit baseline | `c103b74be47920e307b99efe4ff739a1614ec7a1` |
| Final Rust implementation | `5682f8d` plus the service-fixture correction |
| Go soju | `de8690c06fd31e2d34bc84926a6620c5ab2796f0` |
| Ergo | `47b63c9bb58012311274c26c8b950201ec40364e` |
| Local `../gamja` tracked source | `3d16a084b6843b40ce4fcc53583c51d5b7a55562` |
| Local `../senpai` | `33ecee913903616c372c4af5150628d7cf78853f` |

Formatting, locked workspace/all-target/all-feature check, strict GNU/musl
Clippy, 298 tests per GNU/musl target, reader smoke, both doc-test runs and
warning-clean rustdoc pass. Both Tokio runtimes remain covered. The standalone
fuzz package passes formatting/check/Clippy and eight campaigns: IRC, PROXY,
BARE and scfg, 10,000 mutations each with seeds 1592598566 and 3735928559.

Fresh cargo-audit/deny/machete checks could not run: these commands are absent
in this session. Their earlier passing results remain historical evidence;
this run does not refresh the advisory database. No manifest or lockfile changed.

## Release and restored-database checks

`DIST_DIR=/tmp/soju-phase9-dist scripts/build-release.sh` passes for v0.99.25.
Both archives pass `sha256sum -c SHA256SUMS`, extraction and CLI smoke checks:
`soju -version` exits 0; `sojuctl -help` exits 2 with usage. Both musl binaries
have neither ELF INTERP nor dynamic NEEDED entries.

Each extracted archive starts against its own restored SQLite backup, accepts
an admin `user run fixture network status` query and shuts down cleanly on
SIGTERM. The synthetic fixture has schema 35, one disabled network, 60 targets
and 100,000 messages. Backup, fresh-connection reopen and post-shutdown checks
retain all message IDs and the 100,000 high-water ID; integrity, foreign keys
and FTS integrity pass. Historical migration/rollback/concurrent-initialization
tests also pass in the full suites. No production database was used.

## Client and protocol compatibility

| Check | Result |
| --- | --- |
| IRCv3 script in an isolated fresh environment | 32/32 PASS |
| Rust TLS/client certificates/SASL EXTERNAL/PROXY/ident/metrics/reload plumbing | 20/20 PASS |
| Browser scenarios built from local `../gamja`, system Chromium | 4/4 PASS against Rust; 4/4 PASS against pinned Go |
| Local `../senpai`, `SOJU_RS_BINARY=<GNU release>/soju go test ./... -count=1` | PASS; real bouncer test enabled |
| Senpai `go test ./irc -run '^TestSojuRS$' -count=1 -v` with the same binary | PASS, 8.36 seconds |
| Twelve selected Go/Rust differential scenarios | FAIL: all twelve differ |
| Same twelve original/final Rust scenarios | Eleven pass directly; service scenario passes after completion barriers; corrected broadcast scenario also passes |

The browser scenarios exercise adding/editing/removing networks, channel chat
in both directions, history/read markers/search, pins/mutes/detach and
BouncerServ. Senpai's real-bouncer scenario exercises two networks, SASL,
capabilities, multiplex, multiline, history/search, unread/mentions, read markers,
metadata and detaching one network without closing the connection.

The differential selection is registration_bouncer, registration_errors,
channel_basics, chathistory, read_markers, labeled_upstream, bouncerserv,
caps_negotiation, setname_nick, away, invite_and_who_cache and
broadcast_and_user_run. Go differences include Rust extensions, valid CAP
splitting, capability transitions, service/help output and deferred-response
timing. They remain failures of exact transcript equivalence; successful client
tests do not erase them. No incompatibility with the two tested client forks
was demonstrated by these differences.

The original/final Rust service fixture sent updates/status/deletion before
asynchronous user creation completed. PONG did not establish that completion.
The maintained harness now waits for service replies after deferred operations,
and for the specific network-created reply before continuing the broadcast
scenario. Both corrected scenarios were also rerun against Go/Rust; their
remaining Go transcript differences are retained.

Initial socket denials were rerun with approved access. An IRCv3 run overlapped
an environment restart and timed out; the isolated rerun passes all 32 checks.
An initial plumbing invocation selected the config directory as its socket path;
using a separate socket directory resolves startup and all 20 checks pass.
These failed attempts are not counted as passes. Browser build/preview logs
contain missing-git-metadata notices because setup copies tracked Gamja files
without its `.git` directory; the build and all browser assertions pass.

Reproduction uses [tests/compat/README.md](../../tests/compat/README.md), with
`COMPAT_WORK=/tmp/soju-phase9-compat`, the pins above, `GAMJA_SRC=../gamja`
and `CHROMIUM_EXECUTABLE=/usr/bin/chromium`. Run environment-resetting harnesses
sequentially. The Rust comparison substitutes the original Rust release for the
Go executable in an isolated copy of `env-up.sh`; both sides retain separate
Ergo instances and databases.

## Performance and resource measurements

Original/final optimized readers use the checked-in `reader` benchmark with
the same workloads: 65,536 messages at 32/256 bytes and 8,192 at 4,096 bytes.
Each executable warms up, then takes seven samples. Five executable pairs
alternate baseline/final order; the table reports the median and range of
those five per-executable medians, in ns/message.

| Payload | Original median (range) | Final median (range) |
| --- | ---: | ---: |
| 32 bytes | 500.3 (495.9–525.6) | 518.3 (512.5–551.8) |
| 256 bytes | 866.9 (851.9–915.0) | 869.4 (847.4–881.3) |
| 4,096 bytes | 7,014.3 (6,878.7–7,317.5) | 6,504.7 (6,175.8–6,688.5) |

The small-message median is 3.6% slower, the middle workload is similar and
the large workload is faster in this run. These compare the full dependency
and ownership changes; they do not isolate a cause or establish universal
speedups. Reader throughput excludes storage, TLS and actor dispatch.

On one CPU, `reader --actor-boundary` measures the current-thread 10-ms timer
under a 250-ms SQLite lock at 330.584 ms for legacy inline execution versus
11.280 ms for the worker boundary. Multi-thread values are 11.139/11.677 ms.
The cost remains explicit: 4,096 parsed 32-byte messages take 2.088 ms inline
versus 25.931 ms with worker dispatch on the current-thread runtime, and
2.326/20.766 ms on the two-worker runtime pinned to that CPU. This synthetic
handoff cost is the documented responsiveness tradeoff, not production capacity.

`reader --receipt-clients` still shows the enumeration tradeoff: one client's
16 rows take 0.554/1.855 microseconds for the old/set algorithms; 4,096 clients
take 403.140/3.085 ms. This models enumeration in memory, not the bounded
persistent-cache path or a whole-user memory budget.

The file-backed SQL fixture uses the existing connect-pages generator with seed
1, 100,000 messages, 60 targets and 90% events, plus canonical millisecond UTC
timestamps and the maintained FTS schema. Original/final table schema bytes
are identical (SHA-256
`26b8d9cfc9537ad8cea0cc67ac874cc3488ddb967affd57c10247288565045dd`).
Across 26 measured samples alternating the original fixture and its backup,
fresh SQLite connections with a warm kernel cache take 0.484 ms median
(0.461–0.708) for the production-shaped TARGETS SQL and 6.511 ms
(6.256–8.550) to fetch LATEST 100 text rows for each target. This isolates SQL
and Python row-fetch cost; it is not a baseline/final Rust server latency test.

`python3 scripts/bench-connect-pages.py --messages 100000 --targets 60` passes.
Schema-33/current-layout page reads are 1,808/681 at 50% events, 2,386/1,437
at 90% and 793/832 at 99%. This compares historical layouts, not two different
schemas introduced by the audit; it does not show a uniform layout speedup.

The final `scripts/resource-soak.py --seconds 180` rerun passes both workloads
and their five-second drains. The connection/history process completes 1,458
bursts: 23,328 accepted CAP exchanges and 5,832 overload rejections. Every
burst checks 16 live admissions and release to zero before sampling. Retained
history stays within 1,024 targets, 1,279 sampled messages and 5,102,083 accounted
bytes. RSS spans 14,572–29,380 KiB, including setup and drain. The drained state
has zero tasks/sessions/history and ten FDs, versus eleven initial FDs.

Slow push offers 7,931,904 jobs, completes 716 and records 7,931,059 overflow
events. Samples retain at most 128 pending jobs, one active job and 97,780
accounted bytes. RSS spans 7,300–8,264 KiB; tasks/FDs peak at 2/13 and drain to
0/10, with zero pending bytes or active jobs. Coalescing/cancellation mean
offered/completed/overflow counts are not a conservation equation. These are
finite workload models, with the same allocator and scope limits documented in
[resource soaks](resource-soaks.md).

## Retained limitations and evidence

CI-001/002 remain unresolved outside the implementation scope. Exact Go
equivalence, multi-hour production soaks, coverage-guided continuous fuzzing,
real push delivery and public-certificate/reverse-proxy deployments are not
established. The measured actor handoff cost and memory-only receipt retention
remain explicit. Final verification does not remove those limitations.

Local evidence uses `/tmp/soju-phase9-*`: GNU/musl tests and lint logs, archive
and deployment checks, fuzz campaigns, browser/client outputs, Go and Rust
comparison reports, `performance.log`, `connect-pages.log` and `soaks/`.
Temporary drivers are `measure.py`, `dbperf.py`, `deploy.py` and `rust-diff.py`.
Their key settings/results are recorded here because temporary files are not
durable evidence. Compatibility processes are stopped after verification;
no release is published by these checks.
