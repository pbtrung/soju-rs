# Performance measurement

Measure the path a change affects: parsing, history lookup, connection handling
and notification work have different costs. The
[archived review](archive/performance-layout.md) retains the Rust 1.99.1 layout
and reader timing results. The [2026-11-18 final assessment](audit/final-verification.md#performance-and-resource-measurements)
records newer reader, actor, receipt, SQL and resource results. Neither set of
measurements is a universal throughput claim.

## Reader benchmark

From the repository root:

```sh
cargo bench --bench reader
cargo bench --bench reader -- --layout
```

The [reader harness](../src/bench/reader.rs) uses the production reader,
mimalloc, a duplex stream and a current-thread Tokio runtime. It exercises
input permits and checks parsed message counts and payload sizes. Each
optimized workload warms up, then reports the median and range of seven
samples. The `--layout` option prints inline structure sizes instead.

TCP, TLS, SQLite and downstream fanout are outside this benchmark. Its timing
does not establish whole-server throughput. Debug builds run short correctness
checks rather than timing samples, including when Cargo invokes the harness
through `cargo test --all-targets`.

For a before/after comparison, save the baseline and changed optimized
executables, then alternate runs on the same machine after compilation has
finished. Record commit, compiler, target, CPU, allocator, payloads and sample
ranges. Compare equivalent build settings and workloads.

## Actor, receipt and resource workloads

```sh
cargo bench --bench reader -- --actor-boundary
cargo bench --bench reader -- --receipt-clients
cargo bench --bench reader -- --memory-churn
python3 scripts/resource-soak.py --seconds 180 --output "$HOME/soju-soak-results"
```

The actor workload compares legacy dispatch models and bounded blocking jobs,
including SQLite lock contention and parsed-message handoffs on both runtimes.
The receipt workload compares enumeration algorithms on identical in-memory
rows; it does not measure persistent-cache capacity. Memory churn prints
accounted history usage and RSS. The timed soak runner exercises connection
admission/teardown, sender churn and slow-push queue ownership, then checks drain.
Use a new output directory for the samples you intend to keep.

The soaks need local socket access and Linux `/proc` counters. They run as
opt-in optimized tests, outside normal `cargo test`; see
[workloads and interpretation](audit/resource-soaks.md). Finite regression tests,
mutation campaigns and timed resource models answer different questions.

## Storage benchmark

```sh
python3 scripts/bench-connect-pages.py --messages 400000 --targets 60 --limit 100
```

This synthetic workload compares the older rowid message layout with the
current clustered layout. It requires the `sqlite3` shell and uses temporary
databases. See [message storage](message-text-index.md#measuring-storage-changes)
for its output labels and limits. Real deployment measurements should also
record database size, event ratio, cache state and history query latency.

## Memory and layout

Inline structure sizes exclude heap allocations owned by strings, maps,
vectors and shared objects. They vary with compiler and target; use the layout
command above for the build under investigation.

`User` owns network and protocol state, while connection tasks submit events.
Boxing upstream state could reduce the stride of `Vec<Network>` but adds an
allocation and indirection for active networks. Benchmark that access pattern
before changing ownership or layout.

Cache-line size is not a maximum Rust structure size. False sharing concerns
independent data accessed by different cores, with at least one writer; padding
every field or aligning every object is not a general fix. Profile contention
before changing shared counters. See the
[Linux false-sharing guide](https://docs.kernel.org/kernel-hacking/false-sharing.html)
and [Rust layout reference](https://doc.rust-lang.org/reference/type-layout.html).

The [stream reader](../src/conn/transport.rs) reuses partial input across
cancelled reads and releases line-buffer capacity above 1 KiB after a complete
line. That bound covers only the reusable line buffer: buffered I/O, partial
lines, parsed messages, queued output and history also retain memory.

For resource changes, track active tasks, open file descriptors, retained
queue/history bytes and RSS during repeated connection churn and sustained
traffic. Test slow readers, many distinct senders and slow push endpoints.
Per-connection limits alone do not establish a process-wide memory bound;
the [resource guide](resource-limits.md) describes current policies and their
unbounded exclusions.

[Documentation index](README.md) · [Compatibility tests](../tests/compat/README.md)
