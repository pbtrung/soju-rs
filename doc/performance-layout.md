# Memory layout and reader performance review

## Cache lines are not a maximum struct size

A CPU fetches the cache line containing the bytes being accessed, not every
cache line occupied by the enclosing Rust struct. A write marks a line dirty;
it does not necessarily reload it from RAM. False sharing occurs when different
cores access independent data in the same line and at least one core writes.
It can happen inside a small struct as easily as inside a large one.

64 bytes is common, including on the x86-64 machine used for this review, but
is not a portable constant. Rust's default layout can reorder fields; neither
source field order nor a struct smaller than 64 bytes guarantees a particular
cache-line placement. Aligning every object wastes memory and can make array
traversals less efficient.

References: [Linux false sharing guide](https://docs.kernel.org/kernel-hacking/false-sharing.html)
and [Rust type layout](https://doc.rust-lang.org/reference/type-layout.html).

## Observed layout

Measured with Rust 1.99.1 on x86-64. These are inline sizes, not total retained
heap memory, and can change with compiler, target or dependencies:

| Type | Bytes |
| --- | ---: |
| Server | 248 |
| User | 416 |
| Network | 1,920 |
| Event | 200 |
| UpstreamConn | 1,288 |
| DownstreamConn | 1,160 |
| Message | 144 |
| Metrics | 64 |

Reproduce with:

```sh
cargo run --release --example bench-reader -- --layout
```

## Repository findings

* `User` and its networks have one logical owner. Connection tasks send events;
  they do not concurrently mutate channel/member maps. Migrating an actor
  between worker threads may transfer cache ownership, but is different from
  multiple threads continually writing the same state. Shrinking every actor
  to 64 bytes is not justified by its current ownership model.
* `Network` includes its optional upstream state inline. Boxing that state
  would shrink the stride of `Vec<Network>` and save inline space for disabled
  networks. Active networks would gain an allocation and an indirection.
  Benchmark network lookup and dispatch with realistic network counts before
  making that tradeoff; this review does not establish a bottleneck there.
* `Server::metrics` contains adjacent atomic counters updated by different
  tasks. This is a plausible false-sharing candidate at high multicore traffic,
  not a measured bottleneck. Padding each counter also does not remove true
  sharing when multiple cores increment the same counter. Profile before
  choosing padding, sharding or batching. For the proposed one-vCPU deployment,
  this is a lower priority than allocation and storage latency.
* Many map/string contents are already on the heap: boxing the struct that
  contains their headers does not shrink the contents. Channel-member keys
  already use exact-size boxed strings, preserve original spelling only when
  necessary, and use constant-time swap removal. Outgoing queues already store
  formatted lines rather than full message structs.
* The stream reader previously discarded its temporary `Vec<u8>` after each
  line. Ordinary lines can reuse that allocation. The optimization retains at
  most 1 KiB of capacity after a complete line, releases larger buffers, and
  keeps partial bytes across cancelled reads. Parsing produces owned message
  fields before clearing the buffer, including on parse errors.

The retention bound applies to the reusable line buffer only. The existing
8 KiB buffered stream reader, message fields, and partially received lines use
additional memory. This is a bounded space/time tradeoff, not a reduction of
every connection's idle memory footprint.

## Reader benchmark

```sh
cargo run --release --example bench-reader
```

The benchmark uses the production reader, the server's mimalloc allocator,
an in-memory duplex stream, and a current-thread Tokio runtime. It exercises
input permits and checks the number and payload size of parsed messages.
For each payload size it warms up once and reports the median and range of
seven samples. It excludes TCP, TLS, SQLite and downstream fanout, so its
results cannot be interpreted as whole-server throughput improvements.

For a before/after comparison, build and save the baseline executable before
changing the reader, then alternate baseline and changed binaries on the same
machine after both builds have finished. Do not compare an optimized build
against a debug build or run the measurement concurrently with compilation.

The host used here is an Intel Xeon E-2176M. No hardware performance counters
were collected: this is an elapsed-time comparison, not evidence that a
particular cache-miss or invalidation count improved.

Three alternating baseline/changed runs produced these medians (each cell is
the median of that executable's seven samples, in nanoseconds per message):

| Payload bytes | Baseline runs | Buffer reuse runs |
| --- | --- | --- |
| 32 | 489.9, 477.5, 488.8 | 484.9, 485.2, 471.2 |
| 256 | 821.9, 825.1, 821.4 | 801.5, 806.5, 801.3 |
| 4,096 | 6,115.4, 6,089.4, 6,155.3 | 6,227.6, 6,181.9, 6,191.5 |

The 256-byte case improved by roughly 2–3% here. The smallest case was mixed;
the large-line case was roughly 1–2% slower and does not reuse the allocation.
The change removes an allocation for ordinary lines, but is not a universal
throughput improvement. It does not establish a meaningful whole-server speedup
at the proposed 300,000 messages/day, and it retains some extra idle memory.
Measure the actual deployment before making further layout changes.

## Problems with the supplied optimization examples

* `Box<[u8; 64]>` has byte alignment; allocating 64 bytes does not guarantee
  64-byte alignment. A wrapper with explicit alignment would be needed.
* `Vec::remove(head)` shifts elements. Together with wrapping indices and the
  supplied fullness check, the example does not implement a correct ring
  buffer. This repository uses `VecDeque` for its in-memory history instead.
* `_mm256_load_ps` requires 32-byte alignment. Arbitrary `&[f32]` slices do not
  guarantee it. The example also lacks length checks, tail handling, and AVX
  feature checks. See the [intrinsic documentation](https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_load_ps.html).
* A chunk of 64 values occupies `64 * size_of::<T>()` bytes, not necessarily one
  cache line. Separate boxed chunks also introduce allocations and pointers.
* Structure-of-arrays and prefetching depend on the access pattern. A numeric
  particle loop is not representative of an IRC server doing string parsing,
  socket I/O and database operations. No general 20–50% gain follows from these
  techniques.
