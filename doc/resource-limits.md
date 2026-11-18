# Resource limits

## Memory history

The memory message store has per-user aggregate limits of 1,024 retained targets,
16,384 messages and 16 MiB of accounted bytes. Each target also retains at most
4,096 messages. Accounts using SQLite retain their existing database policy.
The memory limits are fixed server defaults; Rust callers can construct a store
with explicit positive `MemoryLimits`.

Accounting includes owned message/string/vector storage, sequence metadata and
target keys. Map/deque spare capacity and allocator bookkeeping are additionally
bounded by the target/message/per-target caps. The byte counter is not a total
process-RSS limit; receipts, network state and other server resources have their
own policies. Oversized records are rejected before changing retained history;
IRC message forwarding continues even if history storage reports an error.

On aggregate pressure the store evicts the target least recently appended to.
If only the current target remains, its oldest messages are removed until the
new record fits. Appending to an existing target refreshes its recency. Reads do
not refresh recency. Deleting a network releases all of its history accounting.

Memory cursors retain the existing BARE/base64 header and sequence encoding,
with a 128-bit store-incarnation token appended to the memory-specific body.
Clients continue treating IDs as opaque. Sequences increase across targets and
network deletion/recreation. Replay filters actual retained sequence numbers,
so an evicted target's old cursor can retrieve newer retained messages without
being mistaken for the recreated target's current cursor. Missing history is
truncated intentionally; it cannot be reconstructed.

Cursors for another conversation, another store incarnation, legacy cursors
without an incarnation token and future sequences are rejected explicitly.
Restarting an in-memory store loses its history; it does not promise replay from
old memory IDs. SQLite IDs, delivery cursors, schema 35 and Go reference codec
vectors remain unchanged.

Run the finite churn/RSS experiment with:

```sh
cargo bench --bench reader -- --memory-churn
```

It alternates sustained target churn with large-record churn and prints retained
usage and Linux `VmRSS` where available. It is a retention experiment, not a
whole-server soak or a throughput comparison.

Measured on 2026-11-17 with the optimized GNU/Linux binary: 800,000 distinct
target appends retained 1,024 messages/targets and at most 848,896 accounted
bytes; sampled RSS stayed between 9,716 and 11,892 KiB. The subsequent 40,000
16-KiB record appends retained 1,010 messages in one target, 16,769,122
accounted bytes, and sampled RSS was 29,796 KiB in all four batches. These
measurements describe this finite workload and allocator, not a universal RSS
ceiling or a server soak result.

## Accepted sessions

`max-accepted-connections` defaults to 256 and must be positive. The budget is
shared by every listener of the server: TCP, Unix, IRC, TLS, HTTP/WebSocket,
Prometheus, admin and ident. Admission happens immediately after accept and
before spawning handshake/session work. At capacity the raw socket is closed;
there is no response or task waiting for a slot. Kernel listen backlogs and
one transient accepted socket per listener are outside the admitted count.

An owned permit wraps the physical stream and remains underneath PROXY/TLS,
HTTP and WebSocket layers. Forwarded identities do not affect accounting.
Upgrades transfer the stream and its permit. Errors, EOF, cancellation and
shutdown drop the stream; a session cancellation guard also closes its writer.
Normal completion still drains final replies. Slow writer cleanup remains
counted until physical teardown (the existing write timeout is ten seconds).

The setting can be reloaded. Lowering it does not evict existing connections;
new admission waits for the count to fall below the new limit. Because admin
and monitoring share the budget, capacity exhaustion also prevents a new admin
or metrics connection. Size the limit for expected clients and available file
descriptors; it does not limit outbound upstreams or replace queue limits.
`soju_accepted_sessions` reports the current admitted physical-session count.

Regressions exercise 300 overload attempts with partial PROXY/TLS/HTTP input,
assert a two-session ceiling and bounded live Tokio tasks, and check release
on close, errors, Unix admin shutdown and explicit downstream-task abort. Other
checks cover concurrent admission, WebSocket upgrade/close, lowering the limit
and shutdown of an unregistered upgraded session. These are finite regressions,
not a whole-server soak. In the isolated GNU/Linux run on 2026-11-17,
each 100-attempt burst kept two admitted sessions, five live Tokio tasks and
17 process file descriptors before and after the burst.

## Delivery receipts

For SQLite message storage, each user's networks share a dirty-receipt cache
limited to 4,096 target/client entries and 4 MiB of accounted bytes. Accounting
charges each entry for client/ID bytes, two target spellings and a conservative
256 bytes of map/string metadata; it is not a process-RSS ceiling. Temporary
flush copies and SQLite's own page/sort storage are outside that retained-cache
counter. The existing 10,000-target cap on client-supplied acknowledgments still
includes durable targets.

Pressure flushes a network's cached rows before releasing them. If other
networks occupy the budget, or an entry alone exceeds the byte limit, that
entry writes through to SQLite. Disconnect flushes and releases only the
client's cached rows. A database failure retains the failed dirty rows for
retry and is logged; it never silently evicts their cursors. Shutdown retries
remaining cached clients through the normal persistence path. As before,
unrecoverable database errors or abrupt process termination can cause repeated
playback; this cache does not promise a durable write for every acknowledgment.

Cache flushes update/insert individual conversations and preserve every
uncached durable row. The old full-replacement database API retains its existing
semantics for callers explicitly requesting replacement. Durable receipt rows
have no new expiry/deletion policy: disk cleanup is a separate retention decision.
Database schema 35 and existing row IDs are unchanged.

Startup does not load delivery receipts. Reconnect flushes that client's dirty
rows and reads at most 128 canonical conversations per page, without retaining
a growing target list or a database lock during playback. Legacy case aliases
use the newest row, matching the old startup loader, and updates cover aliases
together. Registered SQLite scalar functions apply the same IRC mapping and
lossy UTF-8 policy as the eager Rust loader, including legacy Go databases.
Paging can scan/sort a client's durable rows repeatedly; it bounds Rust reload
storage but does not claim constant-cost queries. No new persistent SQL function
index is installed, so upstream Go can still open/write the database.

Memory-only message stores keep their previous receipt lifetime: target/client
cursors remain until network/user teardown, including historical client names.
They have no durable backing to reload after eviction. They are explicitly
outside both the memory-history budget and the durable receipt-cache bound;
no whole-user or whole-process memory bound is claimed. Capping or expiring
these cursors would require a separate replay-retention policy.

Client enumeration now uses set membership while preserving first-encounter
order. Run `cargo bench --bench reader -- --receipt-clients` to compare it with
the former vector membership scan on 16 targets. This synthetic comparison uses
memory-only receipts so both algorithms see identical cardinalities beyond the
durable cache limit. Seven-sample medians measured with optimized GNU/Linux on
2026-11-17:

| Clients | Receipt entries | Old vector scan | Set membership |
| ---: | ---: | ---: | ---: |
| 1 | 16 | 120 ns | 435 ns |
| 8 | 128 | 2.859 µs | 3.123 µs |
| 64 | 1,024 | 109.663 µs | 26.081 µs |
| 512 | 8,192 | 6.266 ms | 0.230 ms |
| 4,096 | 65,536 | 393.526 ms | 3.052 ms |

The set has a small absolute cost for very few clients and removes the
quadratic growth at high cardinality. These timings do not measure end-to-end
notification or reconnect throughput. Regressions separately cover shared
cache pressure, large entries, 10,000-receipt lazy file reopen, partial flushes,
case/invalid-UTF8 aliases, atomic new-target initialization, injected failure
and named-client playback after disconnect/restart on both Tokio runtimes.

## Connection ownership

`Conn` clones and virtual views are session owners. The writer task owns only
transport/state, without a sender or session owner, so it cannot keep itself
alive. Dropping the final owner cancels an abandoned session, including rate
limiter and blocked write waits. The existing writer-close operation then has
a ten-second timeout; idle teardown finishes without that delay. Reads wake on
cancellation. A retained reader still owns its read half until it is dropped;
dropping only the reader does not close remaining writer owners/views.

Calling `shutdown()` before releasing owners explicitly requests graceful,
ordered queue draining. Dropping those owners does not cancel that drain.
Rate limits still apply, so graceful duration depends on queued output and its
configured rate; stalled writes and transport close retain their individual
ten-second timeouts. Calling `close()` requests cancellation instead.

Cancelling a downstream serving task also notifies the user actor to remove
its connection clone and flush client receipts. This uses an owned attachment
guard, ordered after attachment. Tests cover an idle abandoned stream, clones
and views after reader drop, queued and rate-limited draining, blocked close,
rate-wait cancellation and abort after registration on both Tokio runtimes.

## Web Push work

Each user has one notification worker and a pending queue limited to 128
network/conversation keys and 1 MiB of accounted message/key data. One job may
also be active per worker. Admission and replacement happen synchronously
before any per-notification work; there are no per-message sleeper/waiter tasks.
Queue overflow rejects the new optional push and increments
`soju_webpush_overflow_total`, leaving an existing queued state intact. IRC
forwarding continues. `soju_webpush_pending`, `soju_webpush_active` and
`soju_webpush_coalesced_total` expose queue/worker activity and replacements.
These limits cover notification work, not all user/network/subscription state.

Repeated messages for a conversation replace its pending payload within a
five-second window measured from the first message. At delivery the worker
checks the latest durable read marker and suppresses messages already read.
MARKREAD notifications replace pending conversation state and run without the
message delay. A new unread message following MARKREAD starts a new five-second
window. Mandatory IRC delivery does not await notification HTTP or queue space.

All ordinary and registration-test requests share the sender's 32-request
semaphore, acquired before encryption/HTTP. The existing 15-second HTTP timeout
applies after admission. Each user's registration tests retain their separate
four-slot limit; each user/network retains the 25-subscription registration cap.
Global request waiters come only from these bounded workers/test slots, with
no additional task spawned to wait for capacity.

Network deletion cancels its active notification and removes its pending jobs.
User stop cancels optional queued/active work and joins the worker, including
a stop during startup retry. Already-started blocking DB calls may finish;
they cannot recreate subscriptions. A request already received by a remote
endpoint cannot be recalled. Pending work is optional and is not persisted for
retry across shutdown.

Before sending, a queued subscription is re-read using its owner, network, ID
and endpoint. Expiry/pruning deletes only a still-matching owner/endpoint/key
snapshot, so delayed responses cannot delete replaced subscriptions. Successful
deletion invalidates the actor's cached subscription-network set. HTTPS,
external-address checks and credential/payload handling remain in the sender.

Finite regressions flood a stalled mock delivery with 10,000 different targets:
one active job, 128 pending jobs, 9,872 rejected pushes and no task-count growth.
Other tests cover 10,000 coalesced MARKREAD updates, byte pressure, network/user
cancellation, delay transitions, latest read receipts, expiry/replacement/owner
isolation, the full global request semaphore and subscription/test-slot limits
on both Tokio runtimes. The stalled delivery test uses an injected pending
future; it is not a remote endpoint throughput or whole-server soak result.

## Durable mentions

The user actor owns each network's pending mention map and coalesces the latest
timestamp per conversation. One actor deadline flushes due batches after one
second; no detached per-network flush task holds a second copy of live state.
SQLite writes a batch transactionally and only advances stored timestamps.
The actor awaits started blocking work and clears pending state only after a
successful commit. Failed batches remain pending, merge newer timestamps and
retry after five seconds even without another incoming message.

Orderly user stop cancels network activity and flushes every remaining batch,
regardless of its coalescing deadline, before the user completion signal. Stop
makes up to three attempts with 100-ms pauses. SQLite's existing five-second
busy timeout applies to each attempt; total stop time also depends on the
number and size of networks/batches. Permanent errors are logged explicitly
and counted by `soju_mention_shutdown_failures_total`; such a stop may have
incomplete durable mentions. `soju_mention_flush_errors_total` counts failed
batch attempts. Pending mention maps are outside the history/receipt/push
budgets described above; this is a durability policy, not a whole-user bound.

Actor serialization orders a network deletion after any active flush. Removing
a network drops its pending map; foreign keys reject a stale batch if deletion
occurs externally while it waits for a writer lock. Network updates transfer
the pending map and deadline together. Regressions verify stop before a future
coalescing deadline followed by file reopen, transaction rollback, retained
failure/recovery, monotonic timestamps, deletion during a blocked flush and
reported permanent shutdown failures. Current-thread actor-handler database
responsiveness remains the separate STEP-019 work.
