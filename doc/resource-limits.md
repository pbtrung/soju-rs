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
