# Resource, concurrency and design review

[Audit summary](../audit_results.md) · [Record index](README.md)

## 7. Memory Safety and Resource Management

Existing protections worth preserving: 32 KiB input framing cap; 16 MiB accounted physical output queue; stricter 256 KiB unregistered output threshold; 64 ordinary plus four keepalive input permits; 64 pending commands per type; 64 open upstream batches; 64 KiB/1,024-line multiline cap; bounded CPU-heavy work and Web Push registration checks; handshake/registration/write/HTTP timeouts; large line-buffer capacity release; network cleanup of memory history. An unbounded Tokio channel with an enforced byte budget is not automatically an unlimited output buffer.

Remaining gaps: aggregate memory history/receipt cardinality, push tasks, accepted sessions, and writer teardown (MEM-001/MEM-002/ASYNC-001/ASYNC-004). Connection byte accounting uses atomics but does not prove a process-wide bound; message formatting allocates before admission and concurrent sends can transiently allocate above the queued budget. Stress/race tests should measure the practical envelope rather than claim exact RSS from an atomic counter.

No application unsafe code, manual pointer arithmetic or FFI boundary was found. Safe Rust does not prevent the resource exhaustion paths above.

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

ASYNC-005 in [§3](findings.md#3-audit-findings) records the reproduced metric cancellation defect; STEP-003 supplies its small independent fix and regression checks.

Cancellation/shutdown tests must include final durable writes and task completion, not just a cancelled token. See ASYNC-002/003/004 and PROTO-001 for concrete failures/risks.

## 9. Architecture and Refactoring Opportunities

At the original audit, no source file exceeded 800 lines. Highest counts: downstream root 733, service root 666, config 606; upstream registration 525, downstream registration 523, database types 516, history commands 513, ircutil 510 and webpush 505. Existing transport, listener, database, user and upstream module splits are already meaningful.

The original Clippy logical-line counts identified more useful candidates than file size:

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

Other >100-line candidates: welcome, PRIVMSG, BOUNCER, READ, WHO/WHOIS, CLIENTCERT, WebSocket validation, user update and connection runner. These are review prompts, not a demand to split every match/table. Start with history and task ownership changes that directly remove repeated validation and optional-state assumptions. Avoid globally changing `Vec<Network>` layout, adding trait hierarchies, or boxing state without evidence; the [performance guide](../performance-layout.md) explains the tradeoffs.

### Phase 7 implementation boundaries

[Protocol handler ownership](protocol-handlers.md) describes the implemented
boundaries. History uses borrowed wire fields and typed requests/queries, with
service/bouncer shortcuts preceding bound/limit validation. Store algorithms
retain timestamp exclusivity, AROUND splitting, BETWEEN direction and future
clock behavior; READ/MARKREAD stays separate.

Upstream dispatch delegates substantial branches to named handlers and chat
routing is separate from delivery decisions. Fallible connection borrows replace
macro assumptions in these handlers and dispatch/CAP handling. Downstream SASL,
BOUNCER, CAP REQ and welcome phases have cohesive boundaries. Label/batch cleanup,
actor serialization and generation checks retain their established ownership.

The service registry and help lookup live in `src/service/registry.rs`. Typed
callbacks require a user reference for user-scoped commands; global commands
retain their optional caller. Configuration directive validation lives in
`src/config/directives.rs`, grouped in the original validation order. The
refactored functions pass the existing strict lint baseline without new
long-function exceptions. Remaining locally justified dispatcher/READ exceptions
are not a demand to fragment every match or small helper.

## 10. Testing and CI Assessment

The original baseline contained 152 library tests, two sojuctl tests, two listener integrations and 54 server integrations (27 scenarios run on both Tokio runtimes): **210 passing tests per GNU/musl run**. These tests are preserved. Implementation through Phase 8 now passes 298 tests per GNU/musl target: 219 library, one server-binary, two admin-binary, five listener, five parser-property and 66 server integration tests (33 scenarios on both Tokio runtimes). Two timed soak tests are ignored in the normal suite and run explicitly. There are currently zero doctests. `--all-targets` additionally smoke-runs the custom reader harness. Existing coverage includes malformed PROXY/TLS/cert pinning, SASL chunks, credential redaction, stream cancellation, queues, multiline, migration/reopen/FTS, high-water IDs, user isolation, deferred service operations and dual-runtime behavior.

Shell syntax checks and Python AST parsing pass for the release/compatibility/benchmark scripts. These do **not** detect REL-001's missing workflow or prove script operational behavior.

The original audit did not execute external compatibility tooling. Phase 7 executes twelve pinned Go-soju/ergo scenarios and records their raw transcript mismatches; it also compares the original and refactored Rust builds on the same scenarios. Ten Rust comparisons pass directly, and the two fixture-sensitive cases pass after explicit timestamp/completion barriers. This establishes preservation for the measured scenarios, not full Go equivalence. IRCv3/plumbing/gamja remain unrun in Phase 7; results and pinned inputs are in [§12](verification.md#12-verification-matrix).

At discovery, automation and coverage gaps included pull-request/push triggers,
lint/doc gates, tested-commit release dependency, advisory/license checks, parser
corpora and controlled resource measurements. Audit/deny/machete subsequently ran;
Phase 8 added bounded mutation corpora and timed resource measurements. CI
automation remains excluded from implementation, while cargo-outdated and
coverage-guided fuzz tooling were not provisioned. See [verification](verification.md)
for the actual results and scope limits. Use [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) and [Tokio task shutdown guidance](https://tokio.rs/tokio/topics/shutdown) as implementation references; platform/native exclusions must be explicit.

## 13. Compatibility and Performance Risks

- **Database compatibility:** Keep schema 35, high-water ID allocation, clustered history/FTS and lossily decoded upstream text intact. README's introductory “same database” claim is qualified later: older upstream/soju-rs cannot open an upgraded schema 35 database. Preserve/document this existing one-way upgrade; no new migration is proposed just for modernization. Back up before migration validation.
- **Protocol compatibility:** Preserve IRC framing/redaction, SASL failures, labeled-response ownership, batch/multiline semantics, dual-runtime ordering, multiplexing and future server clocks. Bounded history/push/connection policies introduce explicit operational behavior and require documentation and transcript tests.
- **Scheduling:** Moving DB work off the single runtime thread may add handoffs. Admission limits may reject legitimate bursts. Choose limits from measurements and test slow-client fairness; do not create unlimited permit-waiting tasks.
- **Crypto/native:** Keep certificate pinning leaf-only and handshake signature validation; optional client certificates rely on later fingerprint authentication, not CA trust. Do not weaken those intentional trust boundaries during dependency migration. GNU/musl builds are required but do not establish arbitrary-platform support.
- **Measurement:** Original audit measurements describe the original code. Implementation measurements in [resource limits](resource-limits.md) separate retained-state accounting, enumeration, actor handoffs, parsed-IRC traffic and SQLite lock latency. The worker boundary improves current-thread contention latency while adding uncontended throughput overhead; full production capacity remains unmeasured.
- **Rollback:** Dependency changes should be isolated/reviewed; schema remains unchanged. Cursor/eviction and new issuer policy changes need explicit compatibility notes. Keep the last verified working state if a major migration cannot be validated.
