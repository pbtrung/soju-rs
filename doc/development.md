# Development

soju-rs is one Edition 2024 Cargo package with a library and two binaries:
`soju` runs the bouncer; `sojuctl` sends commands to its admin socket. Use Rust
1.100.0 or newer and the native tools listed in [building](../README.md#building).
Run the commands below from the repository root.

## Find the responsible module

| Area | Responsibility |
| --- | --- |
| `src/irc/`, `src/xirc/`, `src/ircutil.rs` | Message parsing/formatting, IRCv3 helpers and IDs |
| `src/conn/`, `src/listen/`, `src/tls.rs`, `src/proxyproto.rs` | Transport, admission, TLS and forwarded identity |
| `src/downstream/`, `src/upstream/` | Client/server commands and connection state |
| `src/user/`, `src/server/` | User events, lifecycle, worker ownership and maintenance |
| `src/service/`, `src/config/` | BouncerServ registry/dispatch and configuration validation |
| `src/database/`, `src/msgstore/` | SQLite/migrations, receipts and memory history |
| `tests/server/`, `tests/listeners.rs` | Protocol/lifecycle and real-socket scenarios |
| `tests/compat/`, `tests/fuzz/` | External interoperability and bounded parser mutation |

## Preserve the ownership boundaries

Mutable user/network state belongs to one user event loop. It transfers
exclusive state to bounded blocking work and awaits completion before handling
the next event. Independent networking and timers stay asynchronous; operations
for that user remain serialized. Preserve runner/connection generation checks
so late events cannot affect replacement connections.

Put protocol changes at that event boundary. Keep reply ordering, labels/batches,
network isolation, SASL, credential redaction, proxy trust and certificate
validation intact. [Protocol handler ownership](audit/protocol-handlers.md)
explains the command and configuration boundaries in more detail.

Admit work before spawning it. Define cancellation, joining and final
persistence when changing task ownership. [Resource limits](resource-limits.md)
explains overload and shutdown policies; accounted bytes are not a whole-process
RSS bound. Synchronous SQLite work must not block the async executor.

Storage changes belong in new migrations. Inspect the migration list; do not
rewrite historical fixtures or lower schema versions to make a test pass.
Preserve IDs, delivery cursors, foreign keys and FTS consistency. Use disposable
databases and consistent backups, never live user data.

## Required checks

For Rust, manifest or lockfile changes:

```sh
cargo fmt --all
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --all-features --locked
```

Run targeted tests while iterating, then the required suite. Server scenarios
cover both current-thread and multi-thread Tokio runtimes; retain both. Socket
tests need local TCP/Unix binding permission. Distinguish environment denials
from test failures and rerun with the required access.

Cargo.toml enforces the compiler/Clippy policy. Fix failures rather than hiding
errors or inventing defaults. Any lint exception must be narrow and reasoned;
keep unsafe code forbidden. [State-lock policy](../src/state_lock.rs) defines
fail-closed actor/auth behavior and separate database rollback recovery.

For Rustdoc changes:

```sh
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
```

For Markdown changes, check relative links/anchors, stale references and
`git diff --check`; a Rust rebuild is unnecessary. For shell changes, use
`bash -n` and ShellCheck when installed. For Python compatibility changes, use
Ruff when available and run affected scenarios; syntax alone is insufficient.

## Native and protocol changes

Native/dependency/release changes require GNU and musl verification. For musl,
set `CC_x86_64_unknown_linux_musl=musl-gcc` and
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc`, and append
`--target x86_64-unknown-linux-musl` to the Cargo check/lint/test/build commands.
See [release builds](releasing.md) for archive/static-link checks.

For dependency changes, run cargo-audit, cargo-deny and cargo-machete when
available. Keep advisory findings visible; see
[dependency maintenance](audit/dependencies.md#reviewing-future-updates).
For protocol changes, use [compatibility tests](../tests/compat/README.md).
For performance or ownership changes, choose the relevant
[benchmark or soak](performance-layout.md) and record its limits.

## Repository workflow

[AGENTS.md](../AGENTS.md) is the repository guidance for automated contributors.
The [commit skill](../.agents/skills/commit/SKILL.md) defines the authorized
commit-and-push workflow. Commit preparation uses formatting and static checks;
it defers compilation and tests to the manually dispatched
[Tests workflow](../.github/workflows/tests.yml). Committing or pushing does not
dispatch CI. Editing documentation does not itself request a commit, push or
release.

[Documentation index](README.md) · [Compatibility](compatibility.md)
