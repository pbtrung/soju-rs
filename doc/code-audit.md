# Code audit and refactor — 2026-11-11

The review focused on stream and WebSocket framing, trusted proxies,
registration and SASL, credential logging, upstream output generation,
message storage and migrations, server lifecycle, and the tests covering
these paths. Findings below were fixed; this report does not establish that
all possible defects have been found.

Persistent storage now uses native SQLite directly, with memory available
for transient message history. Remote provisioning, remote SQL transport,
background message batches, their tests, container benchmark tooling,
configuration directives, CLI bootstrap, metrics, schemas and documentation
were removed. Local SQLite remains at schema version 35; existing local
migrations and message IDs are preserved.

## Confirmed fixes

| Area | Trigger and previous behavior | Change and evidence |
| --- | --- | --- |
| Credential logging | Transport debug logs printed credentials even though registration error logs already used a redaction helper. | Apply redaction to incoming and outgoing transport logs and error paths; cover authentication commands, service commands and messages to authentication services. Output regression verifies secrets are absent. See [framing](../src/conn/transport.rs), [reader](../src/conn/reader.rs) and [tests](../src/conn/tests.rs). |
| PROXY v2 LOCAL | A LOCAL payload could supply certificate metadata, or fail parsing because of an address family that should have been ignored. This required an already trusted proxy connection. | Treat LOCAL payloads as opaque and return no forwarded identity. Tests include certificate TLVs and arbitrary family/payload bytes. See [parser and tests](../src/proxyproto.rs). |
| PROXY validation | Unsupported v2 commands/transports were accepted, v1 address families were unchecked, and the v1 terminator could exceed the length cap. | Reject invalid command, transport and address-family combinations; count CRLF in the cap. Boundary and malformed-input regressions failed before these fixes. See [parser and tests](../src/proxyproto.rs). |
| Forwarded certificate | A trusted proxy header without a certificate erased a certificate obtained from TLS. | Preserve the TLS certificate unless the proxy supplies a replacement. See [accepted connections](../src/listen.rs). |
| Unix sockets | Starting another listener unlinked an existing live socket, making the original listener unreachable through its path. | Probe existing sockets, reject live listeners and reclaim stale sockets. Native tests also verify regular files survive binding attempts. See [binding and tests](../src/listen.rs). |
| WebSocket handshake | Any present handshake key was accepted, including invalid base64 or an incorrect decoded length. | Require a base64-encoded 16-byte key. Real TCP tests exercise rejection and verify the successful handshake response. See [HTTP upgrades](../src/listen/http.rs) and [listener tests](../tests/listeners.rs). |
| WebSocket limits | A WebSocket constructed without the listener's configuration bypassed the IRC message size cap. | Check message size at the reader boundary too; an unconfigured-socket regression exercises this path. See [framing](../src/conn/transport.rs) and [tests](../src/conn/tests.rs). |
| Ident write timeout | Read timeouts did not protect a reply blocked by a peer that stopped reading. | Bound the complete query/reply operation. A paused-time test stalls the output buffer and requires completion within the timeout. See [ident handling and tests](../src/identd.rs). |
| Ident framing | A valid-looking prefix at the input cap could be processed without a complete newline-terminated request. | Reject truncated or overlong requests. Test sends a valid prefix followed by data beyond the cap and requires no reply. See [ident tests](../src/identd.rs). |
| SASL chunks | Oversized individual response chunks were accepted up to the larger aggregate cap. | Enforce the 400-byte chunk limit. Integration tests verify rejection, state reset and successful multi-chunk authentication on both runtimes. See [SASL](../src/downstream/sasl.rs) and [authentication scenarios](../tests/server/authentication.rs). |
| OAuth acknowledgement | The failure acknowledgement accepted either the right length or the right first byte. | Require exactly one byte with value `0x01`. Test checks valid, empty, wrong-byte and extra-byte responses. See [SASL tests](../src/downstream/sasl.rs). |
| JOIN generation | Zipping channels and optional keys silently dropped channels when fewer keys were supplied. | Default missing keys to empty values while preserving key/channel association and sorting. The regression fails on the original code for both empty and short key lists. See [generation and tests](../src/xirc/genmsg.rs). |
| Duration precision | Floating-point parsing lost nanoseconds for large values, mishandled signed zero and could round beyond the supported range. | Parse integer nanoseconds with checked arithmetic and exact fractional truncation. Tests include overflow, long fractions, the maximum supported duration and 1,001 format/parse roundtrips. See [duration parsing](../src/service/duration.rs). |
| Message ID exhaustion | Iterating an unbounded signed range panicked when allocating the last possible ID in debug builds. | Use an inclusive range, checked counts and a guarded high-water update. Tests verify the final ID, rejection after exhaustion, rollback and preservation of stored history. See [message storage](../src/database/sqlite/messages.rs) and [storage tests](../src/database/sqlite/tests/storage.rs). |
| Migration initialization | A schema version read before acquiring the migration transaction could be stale after another initializer completed. | Read the version after BEGIN; repeated initialization leaves data and schema intact. See [database setup](../src/database/sqlite/mod.rs) and [migration tests](../src/database/sqlite/tests/migrations.rs). |
| Shutdown lifecycle | User creation could start a worker after shutdown had captured its user snapshot, leaving that worker unstopped. | Reject creation after shutdown starts and recheck under the user-map lock after storage completes. The post-shutdown regression fails on the original code; concurrency coverage checks that accepted workers stop. See [server lifecycle](../src/server.rs) and [tests](../src/server/tests.rs). |

The storage tests also verify that a batch containing an invalid message
rolls back earlier rows, newly created targets and ID reservations, and
that expiry rejects zero-sized chunks and honors existing cancellation.
If account storage commits while shutdown begins, that account may remain
for the next startup; the fix ensures no worker escapes shutdown's snapshot.

## WebSocket reset log

The native listener regression reproduces:

```text
WebSocket protocol error: Connection reset without closing handshake
```

It verifies that an abruptly dropped client releases its user session and
server connection count, and that the same user can reconnect. It also
checks a graceful close. The reported deployment log alone cannot identify
whether its reset originated in a browser, proxy or network connection.
No change suppresses the transport error or assumes a particular cause.

## Module organization

| Entry module | Original lines | Final lines | Extracted responsibilities |
| --- | ---: | ---: | --- |
| `src/conn.rs` | 915 | 364 | Transport framing, cancellation-safe input, tests |
| `src/listen.rs` | 906 | 351 | Address parsing, forwarded headers, HTTP/WebSockets |
| `src/server.rs` | 812 | 401 | Runtime configuration, administration, maintenance, tests |
| `src/upstream/mod.rs` | 966 | 396 | Connection methods, dispatch, tests |

Public runtime entry points remain available through the existing modules.
SQLite regressions are grouped into accounts, messages, storage, search,
targets, receipts and migrations. Bouncer integration scenarios are grouped
by authentication, protocol, history, read markers, persistence, messages,
multiplexing and services. Each bouncer scenario still runs on both Tokio
runtime flavors.

The custom reader benchmark lives at `src/bench/reader.rs` and is registered
explicitly in Cargo. Run `cargo bench --bench reader`; add `-- --layout`
for structure sizes. Debug builds run short workload checks instead of full
timing samples.

Long SQL and text literals were wrapped using escaped continuations. A
comparison of all normal string literals in changed files confirmed their
contents match the preceding commit after resolving continuations. At this
point no Rust source or test line exceeds 120 characters.

## Validation and limits

- `cargo fmt --all -- --check`: passes.
- `cargo clippy --all-targets --locked -- -D warnings`: passes.
- `cargo test --all-targets --locked`: 210 tests pass: 152 library, two CLI,
  two native HTTP/WebSocket listener tests and 54 bouncer integration tests.
  The benchmark's debug workload checks also pass.
- `cargo test --doc --locked`: passes; there are no runnable documentation tests.
- 22 regression tests were added relative to the remaining suite after the
  remote backend was removed. Several were verified to fail before their fixes.
- Tests use native local servers and in-memory streams; no container tests run.

No deployed browser/proxy environment or live IRC network was used for
validation. External compatibility harnesses, musl builds and production
performance measurements were not run. Dependency-advisory scanning was
not performed: neither cargo-audit nor cargo-deny is installed. The passing
suite establishes the behavior of its exercised cases, not the absence of
all race conditions or deployment-specific failures.
