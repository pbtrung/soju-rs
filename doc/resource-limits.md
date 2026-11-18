# Resource limits and lifecycle

These policies bound particular retained structures or kinds of work. They do
not add up to a whole-user or whole-process memory ceiling. SQLite page caches,
network/channel state, pending mention maps and allocator overhead have separate
costs. The [implementation record](audit/resource-limits.md) contains detailed
accounting, regression coverage and dated measurements.

## Budgets and overload

| Resource | Scope and default | Behavior at the limit |
| --- | --- | --- |
| Accepted physical sessions | 256 per server, shared across listeners | Close the newly accepted socket before spawning session work |
| Memory history | Per user: 1,024 targets, 16,384 messages, 16 MiB accounted bytes; at most 4,096 messages per target | Evict least-recently-appended targets, then old messages as needed |
| Persistent dirty receipts | Per user: 4,096 target/client entries and 4 MiB accounted bytes | Flush cached rows or write through to SQLite; retain failed dirty rows |
| Pending notifications | Per user: 128 network/conversation keys and 1 MiB, plus one active job | Coalesce replacements or reject optional push work and count overflow |
| Push HTTP requests | 32 per sender; registration tests also have four slots per user | Wait through bounded workers/test slots before encryption and HTTP |
| Blocking actor work | 16 jobs per process, admitted before spawning | Await admission while retaining exclusive event ownership |
| Pending upstream commands | 64 active/queued requests per command kind | Reject overflow; an active head has a 30-second deadline |

Only accepted-session admission is exposed as the configuration directive
`max-accepted-connections`; it must be positive and can be reloaded. The other
budgets above are implementation defaults, not configuration directives.
Rust callers can provide explicit positive `MemoryLimits` for a memory store.

Admission counts physical transport lifetime, including TLS/PROXY handshakes,
HTTP and WebSocket upgrades. Virtual multiplex connections share that transport.
Admin, ident and metrics listeners share the same budget; upstream outbound
connections do not. Lowering the setting does not evict existing sessions:
new sockets are rejected until the count falls below the limit. Monitor
`soju_accepted_sessions`; kernel backlogs and transient accepted sockets are
outside that count.

## History and receipts

Database history follows [expiry and maintenance](message-text-index.md#expiry-and-maintenance).
Memory history is intentionally lossy under pressure and disappears on restart.
Reads do not refresh eviction recency. Deleting a network releases its memory
history. A record too large for the budget is rejected without changing retained
history; forwarding can continue after a storage error.

Memory message IDs include a store-incarnation token. Old cursors from another
store/restart, another conversation, legacy format or a future sequence
are rejected. An old cursor for an evicted/recreated target in the same store
can retrieve newer retained messages; it cannot recover evicted history.
Clients must treat IDs as opaque.

The bounded receipt cache applies to SQLite message storage. Eviction does not
delete durable receipt rows; reconnect reads canonical conversations in pages
of at most 128 without holding a growing Rust target list. Query cost can still
grow with durable rows. Failed persistence can cause repeated playback; it must
not silently discard dirty cursors. Memory-only receipt/client-name retention
is unchanged until network/user teardown and is outside these cache budgets.

## Cancellation and completion

Connection clones and virtual views own their session. Dropping the last owner
cancels abandoned work; `shutdown()` requests ordered queue draining, whereas
`close()` cancels it. A retained reader alone can keep its read half alive.
Graceful duration depends on queued output and rate limits; individual stalled
write/close operations have ten-second timeouts.

Push work is optional. A user stop cancels queued/active jobs and joins the
worker; jobs are not persisted for restart. Network deletion removes its jobs.
Durable mentions use a different policy: coalesce updates for one second,
retain failed batches, retry after five seconds and flush on orderly stop.
Shutdown makes up to three attempts with 100-ms pauses; database busy timeouts
and batch sizes still affect completion. Permanent failures are logged and
counted, so shutdown does not guarantee durability after an unrecoverable error.

A timed-out upstream command closes that upstream and aborts its serial queues
with terminal downstream replies. Reconnecting prevents late unlabelled replies
from being assigned to a new request. The 30 seconds starts when the active
head is queued for upstream output; waiting requests can spend longer in a queue.
Timer handling also depends on the current actor event completing.

## Database work and responsiveness

Each user's events stay serialized while blocking work runs off the async
executor. SQLite remains one connection behind a mutex; a slow operation still
delays that user's following events. Started blocking jobs and maintenance are
joined rather than abandoned at shutdown. Admission and coalesced maintenance
avoid spawning unbounded waiters.

This boundary improves independent timer/network responsiveness during SQLite
contention and adds handoff overhead to light handlers. Use
[performance measurement](performance-layout.md) to assess that tradeoff for a
workload. The [final assessment](audit/final-verification.md#performance-and-resource-measurements)
records finite reader, actor, SQL and three-minute resource checks; it does not
establish production capacity or multi-hour stability.

[Documentation index](README.md) · [Configuration reference](soju.1.scd)
