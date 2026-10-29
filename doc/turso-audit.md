# Turso pre-release audit

Audited the Turso implementation introduced by `d478f60`, including its
configuration, controller directory, user routing, Hrana transport, command
bootstrap, expiry and documentation. The v0.99.20 follow-up also audits
background batching, history visibility, request limits and schema metadata. Findings below were addressed before
release. The scope includes local SQLite regressions caused by the shared
connection interface.

| Priority | Finding | Correction and regression coverage |
| --- | --- | --- |
| High | A lost transaction baton could be treated as permission to open another stream, allowing subsequent writes outside the intended transaction. Malformed response types and incomplete result lists were also insufficiently checked. | Validate every pipeline result and stream baton. Fail the transaction after transport/protocol errors, close the last known stream once, and prohibit further SQL until the transaction is discarded. Exercise lost streams, missing results, wrong response types, empty bodies, failed BEGIN and a committed write whose response is lost. |
| High | Startup recreated a missing `ready` user database as an empty database, masking loss of upstream configuration and history. | Only `provisioning` accounts may create missing databases. A missing ready database stops startup with a restore instruction. Test that no replacement database is created. |
| High | The shared schema-31 migration sent `ANALYZE`, which the hosted SQL parser does not accept. A SQLite-only mock hid that incompatibility. | Keep the partial-index DDL but omit the ANALYZE step for Turso. The mock now rejects the known hosted maintenance restrictions, and the schema-30 upgrade preserves messages. Local SQLite still analyzes the new index. |
| Medium | Blob decoding required padded base64, while the actual Hrana implementation emits unpadded base64. This breaks some certificate and private-key lengths. | Match Hrana's encoding and accept padded or unpadded input. Make the mock's decoder independent and test a 64-byte blob. Preserve named parameter prefixes so `:name`, `@name` and `$name` remain distinct. |
| Medium | New streams did not carry the previous write's replication index, allowing reads to miss recent writes on replicas. | Track the highest observed index and include it in subsequent requests. Sequence operations collect an index on their own stream before it closes. Verify the subsequent read sends the write's index. |
| Medium | Remote event handlers blocked the current-thread runtime; the HTTP worker serialized all requests, and account provisioning held the whole user-directory lock. | Run remote handlers and deferred creation on blocking workers, serve independent HTTP streams concurrently, and keep the directory lock out of cloud I/O. Test timers, an existing user lookup during provisioning, and a fast HTTP request while another response is delayed. |
| Medium | A changed group, corrupt link, or update using another user's network ID could operate on the wrong database or poison the routing cache. | Validate link names and owners, record the controller/owner in each user database, check existing network ownership before updating, and recheck the group before deletion. Test corrupt links, owner mismatches, moved groups, and cross-tenant updates after restart. |
| Low | The offline bootstrap bypassed the admin-socket argument checks. Some cloud failures during readiness probing were not retried. | Reject CR/LF/NUL before connecting and retry only readiness reads, including transport failures. Keep general SQL writes and commits non-retryable; v0.99.20 adds a separate idempotent buffered-message writer. |

## Message expiry and maintenance

Turso startup and recurring expiry delete old messages and optimize the FTS
index. Neither path sends `VACUUM`, `PRAGMA wal_checkpoint`,
`PRAGMA shrink_memory`, page-count or freelist probes. Backend-level vacuum
and shrink methods also remain no-ops for Turso. Regression tests inspect the
actual captured HTTP SQL for both startup (`force_vacuum = true`) and
recurring expiry, including retention of fresh messages.

This restriction was checked against the official server's
[statement classifier and pragma allowlist]. It rejects VACUUM and ANALYZE
statements, and explicitly excludes WAL checkpoints, PRAGMA optimize and
PRAGMA shrink_memory. FTS optimization uses its supported INSERT command;
it is distinct from `PRAGMA optimize`. Local SQLite retains its existing
vacuum, checkpoint and index-analysis behavior.

The updated setup and man pages describe maintenance limits, recovery of a
missing ready database, controller backups, ownership checks, credential
separation and ambiguous write outcomes.

## Background batching follow-up (v0.99.20)

- Removed one transaction and several HTTP round trips per incoming line.
  Per-user filling and committing buffers use `chathistory-limit`, a
  one-second partial flush, and an 8 MiB filling-buffer data budget.
- Kept writer streams independent of read connections and their mutexes.
  Shared replication watermarks and an in-memory SQLite/FTS overlay preserve
  pending history, search, target timestamps, ordering, and receipts.
- Reserved cursor IDs durably, and prefetched future ranges on a separate
  stream so ID allocation cannot queue behind a commit's mutex. Exact
  contents are checked on retries; conflicting IDs abort the request.
- Capped complete encoded HTTP bodies at 512 KiB. Large buffers split into
  independently atomic requests. Payload text uses bound blobs cast to text
  to avoid worst-case JSON control-character expansion. Simulated HTTP 413
  and transaction-timeout limits verify adaptive splitting and stable IDs.
- Retained failed batches with backoff. Full buffers defer message events
  while preserving input permits; control events remain serviceable. The
  two-buffer limit does not guarantee unlimited intake during an outage.
- Prevented deletion races by making queued inserts conditional on the
  network still existing. Deletion does not need to wait for the writer's
  HTTP response, and subsequent flushes cannot resurrect that network.
- Reproduced an existing, empty controller with the read-only pragma
  restriction and verified successful initialization and admin creation.
  Startup database errors now include the underlying error chain instead
  of only `initialize Turso controller`.
- Moved cloud schema tracking to `SojuSchemaVersion`: current cloud docs
  mark `PRAGMA user_version` read-only. Legacy versions are imported once,
  while local SQLite continues using its existing pragma.
- Documented forwarding before persistence, crash-loss exposure, limited
  shutdown retries, cloud request budgets, and read-query latency.

## Hosted stream lifecycle follow-up (v0.99.21)

The hosted [SQL-over-HTTP stream rules] allow the server to release a
stateless stream after COMMIT, returning a null baton without an explicit
close. The previous client incorrectly required a baton after COMMIT, so
controller schema creation succeeded but startup reported a transaction
failure. The mock previously retained every stream until explicitly closed.

COMMIT now sends the replication-index probe and explicit close in the same
pipeline, validating every response before reporting success. Transaction
poisoning remains active throughout: lost streams before COMMIT, incomplete
or mistyped responses, and lost commit responses still fail without replay.
The mock now closes idle streams by default. Regressions cover both hosted
and legacy retention behavior, controller and tenant creation and restart,
and malformed commit responses without duplicate writes. Existing schemas
need no reset or migration for this fix.

v0.99.21 validation: formatting and Clippy passed; all 193 tests passed
(156 library, 3 sojuctl, and 34 bouncer integration tests).

## Query-cost and tenant-schema follow-up (v0.99.22)

The [optimization plan](turso-query-optimization.md) was written before the
implementation. The [evidence report](turso-query-evidence.md) records real
libSQL counters and reproducible native plans; no live cloud credentials
were used. The benchmark runs the production writer/receipt/expiry methods
against a pinned, disposable localhost engine and asserts the improvements.

- Multi-row VALUES and a bounded target-ID cache reduce repeated lookups.
  Foreign keys remain enabled; an invalidated cached target rolls back the
  batch, refreshes routing and omits removed networks on its safe retry.
- Identical message retries no longer update rows or run FTS triggers.
  Different contents at a reserved ID still abort rather than overwrite.
- Text/event timestamp indexes are disjoint. Event playback merges both
  ordered ranges; cursor ordering and ties remain covered by regressions.
- Sparse FTS uses columnsize=0 and indexes text rows only. Chunked migration
  saves progress in the same transaction as each range. Cursor predicates
  prevent duplicate indexing after an ambiguous response. Tests interrupt
  migration after a committed chunk and verify resumed FTS integrity.
- Expiry seeks per-target timestamp ranges. Saving the high-water ID now
  seeks one row instead of aggregating the entire message table.
- Receipt deltas preserve unchanged IDs and write only changes. Repeated
  account, network, channel, certificate, target and read-receipt updates
  skip equal values. Unique-index prefixes replace redundant indexes;
  selective certificate, receipt, subscription and target-flag indexes are
  added where their query plans justify them.
- Tenant User, DeviceCertificate and WebPushConfig tables are removed.
  Networks/subscriptions reference TursoOwner.user. Ownership validation
  precedes migration; unexpected tenant authentication records stop it.
  A regression preserves upstream credentials, channels, subscriptions,
  receipts and history through migration and restart. FK checks cover the
  rebuilt tables instead of scanning every message at startup.
- Controller schema 2 and tenant/local schema 32 reject older binaries.
  Migration may incur substantial one-time usage and startup time; backups
  and a sufficiently long startup probe are documented.
- Observed row-usage metrics sum received execute and batch responses.
  They exclude unobserved/lost responses and are explicitly not invoice
  totals. BEGIN/COMMIT execute responses avoid extra SELECT 1 probes while
  preserving the replication watermark and stream validation.

v0.99.22 validation: formatting and strict Clippy passed; all 198 ordinary
tests passed (161 library, 3 sojuctl, 34 bouncer integration), plus the
separately invoked real-engine benchmark. Its ignored test is driven by
`python3 scripts/bench-query-cost.py`.

## Validation and limits

Post-release verification exercises `upstream::append_log` on a running
Turso user's event loop with `chathistory-limit 200`: 400 short messages
produce exactly two HTTP message batches, each with 200 INSERTs and one
BEGIN/COMMIT pair. No message INSERT is sent as a standalone execute
request. Existing timer coverage verifies that a quiet connection can
instead flush a single message after one second. The schema-upgrade
regression also inspects fresh and upgraded tenant databases for the
partial MessageTextIndex and checks text-only history and latest-target
query plans. Formatting, strict Clippy and all 194 tests passed (157
library, 3 sojuctl, 34 bouncer integration tests).

Run the required checks:

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

v0.99.20 validation: formatting and Clippy passed; all 191 tests passed
(154 library, 3 sojuctl, and 34 bouncer integration tests). The earlier
v0.99.19 audit passed its then-current 176 tests.

Turso regressions use the production HTTP client against an independent
Platform/Hrana mock backed by SQLite, with hosted maintenance restrictions
and injected transport/protocol faults. The tests also exercise the actual
server user event loop on a current-thread runtime. Existing bouncer
integration tests continue to cover both Tokio runtime flavors.

Protocol checks used the official [Hrana specification] and [value codec],
including stream invalidation rules, named parameters and unpadded blobs.
The [Platform API authentication documentation] confirms that group SQL
tokens and provisioning API credentials are separate.

Live Turso credentials were not available. These tests do not establish the
behavior of a particular hosted deployment, actual replica lag, account
quotas or production latency. A lost response after a successful general SQL commit is
inherently ambiguous; that path reports the error without replay. Buffered
message writes use reserved IDs and identical-content checks, allowing
retries without duplicate rows. The controller cannot be reconstructed from the random user database
names, so recovery requires its backup as well as the user databases.

[statement classifier and pragma allowlist]: https://github.com/tursodatabase/libsql/blob/main/libsql-server/src/query_analysis.rs
[Hrana specification]: https://github.com/tursodatabase/libsql/blob/main/docs/HRANA_3_SPEC.md
[value codec]: https://github.com/tursodatabase/libsql/blob/main/libsql-hrana/src/proto.rs
[Platform API authentication documentation]: https://docs.turso.tech/api-reference/authentication
[SQL-over-HTTP stream rules]: https://github.com/tursodatabase/turso/blob/main/serverless/PROTOCOL.md#43-stream-retention
