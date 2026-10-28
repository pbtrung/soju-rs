# Turso pre-release audit

Audited the Turso implementation introduced by `d478f60`, including its
configuration, controller directory, user routing, Hrana transport, command
bootstrap, expiry and documentation. Findings below were addressed before
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
| Low | The offline bootstrap bypassed the admin-socket argument checks. Some cloud failures during readiness probing were not retried. | Reject CR/LF/NUL before connecting and retry only readiness reads, including transport failures. Keep SQL writes and commits non-retryable. |

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

## Validation and limits

Run the required checks:

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Result: formatting and Clippy passed; all 176 tests passed (139 library,
3 sojuctl, and 34 bouncer integration tests).

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
quotas or production latency. A lost response after a successful commit is
inherently ambiguous; the client reports the error and does not replay the
write. The controller cannot be reconstructed from the random user database
names, so recovery requires its backup as well as the user databases.

[statement classifier and pragma allowlist]: https://github.com/tursodatabase/libsql/blob/main/libsql-server/src/query_analysis.rs
[Hrana specification]: https://github.com/tursodatabase/libsql/blob/main/docs/HRANA_3_SPEC.md
[value codec]: https://github.com/tursodatabase/libsql/blob/main/libsql-hrana/src/proto.rs
[Platform API authentication documentation]: https://docs.turso.tech/api-reference/authentication
