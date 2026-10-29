# Turso query optimization

## Objective

Reduce rows read and written by Turso while preserving authentication,
tenant isolation, message ordering, history/search results, stable message
IDs, batching, retry safety, and deletion/expiry behavior. Audit every SQL
operation, including startup and provisioning, rather than adding indexes
to every predicate. Additional indexes also have a storage and write cost.

This document was created before implementation. The final findings below
record accepted and rejected changes, measured costs and remaining limits.

## Work plan

Insert cost is the first implementation priority: compare actual engine
rows_read/rows_written for 200-message batches, grouped inserts, identical
retries, target resolution, foreign keys, and FTS. Schema redesign is within
scope when measurements justify it and migrations preserve existing data.

The tenant schema now has no `User` table. Its former references point to
`TursoOwner.user`; authentication remains in the controller. The migration
preserves networks, subscriptions, credentials, receipts and messages.
Local SQLite keeps its combined account schema.

1. Verify current Turso accounting documentation and the SQL engine's
   supported operations. Separate hosted billing evidence from local
   measurements; SQLite VM steps are not a cloud billing estimate.
2. Inventory controller and tenant queries in the connection adapter,
   routing layer, SQLite backend, message writer, and pending-history merge.
   Identify frequency, bounds, existing indexes, and avoidable writes.
3. Build reproducible fixtures with many messages, event-heavy history,
   multiple targets/networks, sparse matches, and expired/fresh rows.
   Capture EXPLAIN QUERY PLAN and execution counters before changes.
4. Prioritize expensive scans, repeated history reads, write amplification,
   expiry, receipt replacement, message-ID allocation, and retries. Prove
   semantic equivalence, including ties, NULLs, limits, and out-of-order
   timestamps, before accepting a query rewrite.
5. Add or remove indexes only with evidence of the resulting read/write
   tradeoff. Provide migrations for existing databases, update fresh
   schemas, and keep cloud-only maintenance restrictions intact.
6. Run formatting, strict Clippy, relevant regressions, the full suite,
   and reproducible before/after measurements. Record fixture sizes,
   plans/counters, limitations, and changes that were deliberately rejected.

## Audit inventory

| Area | Result and remaining work proportional to data |
| --- | --- |
| History and target lists | Text/event indexes form disjoint timestamp ranges; ordered event playback merges them with one final LIMIT. ID cursors retain the target/ID index. TARGETS still examines targets, using bounded latest-message seeks. Pending merges retain the all-target snapshot to preserve max-time/bounds semantics. |
| Text and sender search | FTS supplies candidate IDs; network/target/time and sender filters remain enforced. Rare sender-only matches can require scanning more messages than the result limit. No extra per-message sender index was added without evidence that its ongoing write cost is justified. |
| Append and retry | Multi-row VALUES, bounded target-ID cache, enabled foreign keys, one atomic request, and no-op conflict guards. Invalidate/re-resolve on a deletion race; skip removed networks. Payload/time limits still split requests. |
| ID allocation | Reservations use primary-key MAX seeks and one durable high-water row. The deletion high-water aggregate with HAVING is replaced by an ordered LIMIT 1 seek and a guarded update. No per-message counter write was added. |
| Expiry and deletion | Expiry probes existing text/event time indexes from targets, avoiding a scan of all fresh messages. LIMIT still bounds deletion. Network/user deletion uses indexed foreign keys and primary keys, without a new all-message time index. |
| Receipts | Client-scoped composite index, one snapshot, only changed/removed/new rows written, stable IDs for unchanged targets. Cloud deltas use one atomic batch. NULL clients and unusual duplicate inputs retain their prior semantics. |
| Accounts and certificates | Username/fingerprint/ID lookups already have unique/primary indexes. Add user index for certificate listing, deletion and FK checks; skip no-op account/certificate updates. Listing all accounts and inactivity selection still examine the relevant users. |
| Networks and channels | Existing leading columns of unique indexes cover owner/network queries and FK checks. Remove duplicate single-column indexes and guard unchanged updates. |
| Web Push | Replace overlapping user/network indexes with user+network and the existing network+endpoint unique index. Keep updated_at refresh semantics. No-op equal records skip UPDATE; list-all/distinct operations remain proportional to the subscriptions examined. |
| Controller/provisioning | Keep indexed link/owner checks and read-only schema metadata. Guard repeated deletion tombstones and schema-version writes. Tenant stats no longer query User. Routing cache misses use PK probes, never cross-tenant scans of message tables. |
| Connection adapter | BEGIN/COMMIT use execute responses, which include replication indexes, avoiding extra SELECT 1 probes. Keep the probe for multi-statement sequence writes. Usage metrics count received execute/batch counters once. |
| Migration | Fresh and upgraded schemas agree. User removal checks only rebuilt tables' foreign keys. Sparse FTS copies text ranges in bounded, resumable transactions, with durable cursor predicates preventing duplicate indexing on retry. |
| Stats and maintenance | Accurate COUNT/list-all operations are retained; maintaining counters would add writes to routine mutations. No extra aggregate tables, global expiry index, VACUUM or cloud ANALYZE were introduced. |

## Evidence requirements

- Keep the workload generator and measurement commands reproducible in the
  repository. Compare identical data and query parameters before and after.
- Report actual row counts returned/changed, query plans, full-scan/sort
  counters and VM steps where available. Use hosted rows_read/rows_written
  only when those values come from a real engine response; do not invent
  billing counters in the cloud mock.
- Include index maintenance and FTS effects in write experiments, rather
  than treating an index as free or equating one HTTP batch with one row
  written.
- Test fresh and upgraded schemas. Retain local SQLite behavior unless a
  shared optimization is proven safe for both backends.
- Do not run VACUUM, ANALYZE, checkpoints, or unsupported pragmas on Turso.
- Do not use production credentials or mutate production data for this
  audit. Live hosted validation, if unavailable, remains an explicit limit.

## Results

These exploratory measurements were taken while designing v0.99.22, before
its final code; the [evidence report](turso-query-evidence.md) is
regenerated from the final code and differs by a row or a few VM steps.
The initial local libSQL measurements use the official server image pinned
to `sha256:6dd3eb276d9d3604e4a48ac4a999a2e267814732d57d7e94c04ba71482333a67`.
These are real engine counters on disposable local data, not cloud invoices.

| 200 text messages | Rows read | Rows written | Decision |
| --- | ---: | ---: | --- |
| Current INSERT SELECT per message | 800 | 1,000 | Baseline |
| Identical retry using the current UPDATE | 600 | 600 | Remove the redundant UPDATE |
| Identical retry with a no-op conflict guard | 600 | 0 | Accepted; retry/constraint regressions pass |
| One grouped INSERT SELECT using a VALUES CTE | 801 | 1,000 | Reject as a row-cost optimization |
| Direct multi-row VALUES with an already resolved target | 600 | 1,000 | Accepted with bounded cache and deletion-race recovery |
| Deferred FTS via an extra persistent batch marker | 1,000 | 1,000 | Reject: adds reads and storage without reducing writes |

Disjoint text/event time indexes reduce the direct-VALUES text-message
experiment to 600 reads and 800 writes. FTS `columnsize=0` by itself did not
reduce these billable counters; its purpose in the proposed redesign is to
support a sparse text-only index without per-document size bookkeeping.
Sparse NULL-text indexing passes FTS integrity-check with columnsize=0 and
fails with the previous default. Any existing FTS rebuild must therefore be
resumable and bounded, rather than changing triggers over the old index.

The exploratory high-water seek reduces a 100,000-message local SQLite
measurement from 400,029 to 29 VM steps. Expiry with no matching rows drops
from 300,008 to 514 VM steps by probing the existing per-target time index
from 100 targets. VM steps are not billable-row measurements.


## Final changes and evidence

The complete production-writer and receipt measurements are in
[turso-query-evidence.md](turso-query-evidence.md). Reproduce them with:

```sh
python3 scripts/bench-query-cost.py --output doc/turso-query-evidence.md
```

This starts a disposable localhost libSQL container with a pinned image and
runs an ignored Rust integration test using the actual Connection,
MessageWriter and receipt/expiry methods. No cloud credentials are needed.
The baseline insert SQL and schema are frozen fixtures from commit bc12089.
Cold target resolution is included. Identical retries must write zero rows;
insert/read/write and expiry improvements are asserted by the benchmark.
The script also records native SQLite plans for the major query families.

The indexed expiry strategy costs work proportional to targets plus expired
rows. A database with very many empty targets and very few messages can be
less favorable than the former message scan. It avoids an extra index write
on every message, and the retained target metadata still has product meaning
(flags and retained conversations), so it is not deleted just to tune a scan.

Read costs cannot become one row per message in general: primary-key conflict
checks, foreign keys, index operations and FTS still do work. A schema that
removed constraints would weaken deletion/retry safety. Extra batch-marker
writes and aggregate tables were rejected when they did not reduce measured
cost enough to justify their write amplification and recovery complexity.

Index creation and sparse-FTS copying are one-time migration costs. Large
histories need sufficient quota and startup time. The FTS copy is bounded;
index creation itself is database DDL and remains subject to hosted limits.
An interruption does not require deleting user databases or messages.

Current accounting guidance was checked against [Turso usage and billing]
and its [indexing guidance]. SQLite's [FTS5 columnsize option] explains the
sparse index's token-count behavior. These sources guide the audit; the
before/after numbers are measured locally rather than inferred from prices.

[Turso usage and billing]: https://docs.turso.tech/help/usage-and-billing
[indexing guidance]: https://turso.tech/blog/tips-for-maximizing-your-turso-billing-allowances-48a0fca163e9
[FTS5 columnsize option]: https://www.sqlite.org/fts5.html#the_columnsize_option

## Schema 33: clustered tenant messages

v0.99.22 still wrote three secondary entries per message besides the row:
a text or event timestamp index, `Message_target_index`, and (for text) an
FTS entry. Candidate layouts were measured on the pinned engine with 200
messages per transaction (rows read / written):

| Layout | Text | Events | Rejected because |
| --- | ---: | ---: | --- |
| v0.99.22 rowid table, 3 indexes | 600 / 800 | 600 / 600 | Baseline |
| `WITHOUT ROWID` key `(target, id)`, two time indexes | 400 / 600 | 400 / 400 | An event still writes a time index |
| `WITHOUT ROWID` key `(target, time, id)`, no index | 400 / 400 | 400 / 200 | Last-ID, backlog cursor and search scan a whole target |
| Key `(target, time, id)` + text-only `(target, id)` index | 402 / 600 | 400 / 200 | Accepted |

The accepted layout keeps every frequent query a seek: event history,
latest target time, expiry and deletion on the primary key; last text ID,
backlog after a cursor and search results on the text ID index. Text-only
history and text-only target lists read past interleaved events instead of
using an index: extra reads, no writes. Delivery cursors become last text
IDs, which is equivalent because backlog replays text only.

Rejected alternatives: storing a per-target last ID would add a write per
target per batch; an expression index on `(target, text IS NULL, time)`
costs the same writes as the partial pair; time-bounded backlog queries
would change cursor semantics for out-of-order server times. Local SQLite
keeps its upstream-compatible rowid schema, so the shared queries were
checked against both layouts (and the in-memory pending overlay).
