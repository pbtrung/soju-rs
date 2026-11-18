# Bounded parser fuzzing

This standalone package runs deterministic mutation campaigns against the real
IRC/tag, PROXY, internal-ID/BARE and scfg parsers. It uses stable Rust without
instrumentation. It is not coverage-guided fuzzing and does not establish
coverage completeness; fixed semantic properties live in
[parser_properties.rs](../parser_properties.rs) and the internal storage tests.

Every input is at most 4,096 bytes, each mutation makes at most eight edits, and
a campaign accepts at most one million mutations. Seed files are replayed first. Corpus files are marked binary to preserve
exact wire bytes, including CRLF, across checkouts.
PROXY input is an EOF-terminated byte slice, so declared lengths cannot stall on
network I/O. The BARE target exercises the actual message-ID decoder both from
raw bytes encoded as base64url and from directly supplied cursor strings.

Run all campaigns after implementation, from the repository root:

```sh
cargo check --manifest-path tests/fuzz/Cargo.toml --all-targets --locked
for target in irc proxy bare scfg; do
    cargo run --release --manifest-path tests/fuzz/Cargo.toml --locked --bin "$target" -- 10000 1592598566
done
```

The final argument is a nonzero decimal seed. Repeating target/seed/count and
the corpus reproduces the same inputs. A panic writes a reduced input under
`artifacts/` after at most 128 reduction attempts. Replay it with:

```sh
cargo run --manifest-path tests/fuzz/Cargo.toml --locked --bin irc -- --replay tests/fuzz/artifacts/irc-SEED-ITERATION
```

If a failure is found, retain the minimal reproducer under `corpus/<target>/`
with an explanatory regression and rerun it. Arbitrary generated artifacts and
build output are ignored. The checked-in seeds include truncated/overflowing
lengths, PROXY signatures, Unicode/tag escaping and malformed configuration.
No crashing corpus is fabricated when a campaign finds no failure.

The `irc/invalid-command-marker` seed is a four-byte minimized harness failure:
` @ x`. The compatibility parser accepts `@` as an unusual command after a
leading space, and serializing that token makes it look like a tag marker.
Canonical roundtrip assertions therefore apply only to lexical IRC commands
(ASCII letters or three digits); malformed command tokens are still parsed.
This corrected a harness assumption and did not change parser compatibility.
