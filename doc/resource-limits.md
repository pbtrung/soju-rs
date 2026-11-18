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
