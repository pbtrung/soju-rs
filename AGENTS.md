# Repository guidance

## Project and layout

soju-rs is an AGPL-3.0 Rust IRC bouncer aiming for upstream soju protocol and
configuration compatibility, with documented Rust-specific extensions.
It is one Edition 2024 Cargo package with a library and two binaries:
`soju` (server) and `sojuctl` (admin socket client).

| Area | Responsibility |
| --- | --- |
| `src/irc/`, `src/xirc/`, `src/ircutil.rs` | IRC framing, tags and extensions |
| `src/conn/`, `src/listen/`, `src/proxyproto.rs`, `src/tls.rs` | Stream/WebSocket transport, listeners and trust boundaries |
| `src/downstream/`, `src/upstream/` | Client/server protocol handling and connection state |
| `src/user/`, `src/server/`, `src/service/` | User event ownership, server lifecycle and BouncerServ |
| `src/database/`, `src/msgstore/` | SQLite migrations, persistence and memory history |
| `tests/server.rs`, `tests/server/`, `tests/listeners.rs` | Integration harness and scenarios |
| `tests/compat/` | Go soju, ergo, IRCv3 and gamja compatibility tooling |
| `doc/`, `scripts/` | Maintained documentation and build/release/benchmark tooling |

Start with [doc/README.md](doc/README.md) for documentation. Historical
investigations are under `doc/archive/`; their old measurements and commands
are not current implementation requirements.

## Working constraints

- Preserve IRC reply ordering, labels/batches, network/user isolation, SASL,
  credential redaction, trusted proxy checks and certificate validation.
  Document intentional differences from upstream in README or the relevant
  protocol guide.
- Mutable user/network protocol state is owned by the user event loop.
  Prefer its event boundary over introducing shared mutable state. Preserve
  connection/runner generation checks that reject stale events.
- Bound buffers, queues and background work before spawning tasks. Define
  cancellation, cleanup and final persistence behavior when changing task
  ownership. Avoid blocking the async executor with synchronous SQLite work.
- Keep schema changes in migrations and preserve message IDs, delivery
  cursors and FTS consistency. The current schema is 35; inspect the migration
  list rather than assuming this number remains current. Do not edit old
  migration fixtures to make a new migration pass. Never test against a live
  user database.
- Fix errors rather than replacing them with arbitrary defaults. Avoid new
  unsafe code or broad lint suppression. Prefer a narrow, reasoned
  `#[expect(..., reason = "...")]` when an exception is warranted.
- Respect the requested scope and existing checkout changes. Do not run
  release helpers, commit, push, move tags or publish without authorization
  for that action. Never add private keys, local databases, credentials,
  `target/` or `dist/` to commits.

The authoritative audit checklist is
[doc/audit/implementation-plan.md](doc/audit/implementation-plan.md), with the
[summary](doc/audit_results.md) as its entry point. Update finding/checklist
statuses only after verification.
The audit prompt's audit-only restriction belongs to that explicitly invoked
workflow; it does not prohibit ordinary authorized repository changes or
automatically require a full audit for every task.

## Build and verification

Use Rust 1.100.0 or newer, the minimum declared in `Cargo.toml`. Distro Rust
is supported; do not assume rustup is installed. Native dependencies build
bundled SQLite, AWS-LC and mimalloc and require a C compiler plus CMake, Make
and Perl.

For Rust, manifest or lockfile changes, run:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --all-features --locked
```

Format changed Rust code with `cargo fmt --all` before the formatting check.
Run targeted tests while iterating, then the full relevant suite. Server
scenarios use both current-thread and multi-thread Tokio runtimes; retain both.
Socket tests need permission to bind local TCP/Unix sockets. Report sandbox
failures separately and use the available approval mechanism to rerun them.

`Cargo.toml` enforces the complete practical compiler/Clippy baseline after
STEP-011, including unsafe prohibition, pedantic, numeric/indexing, unwrap/expect,
must-use/future and error-mapping checks. Keep exceptions on the specific
statement, block, parameter or test that needs them, with an explicit reason.
Current exceptions describe validated actor/IRC indices, bounded parser cursors,
fixed UTF-8/constants, cancelled deliveries, the final stderr sink, deliberate
fixture failures and selected ordered dispatchers. Phase 7's history, protocol,
service and configuration boundaries are documented in
[protocol handler ownership](doc/audit/protocol-handlers.md). Keep those responsibilities
separate; never add crate/module-wide suppressions.

`src/state_lock.rs` defines fail-closed actor/auth poison policy. SQLite separately
recovers after transaction rollback, with regression coverage. Operational errors
must be handled or propagated.

- Rustdoc changes: run `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked`.
  Strict rustdoc passes after STEP-011; retain a warning-clean documentation build.
- Markdown-only changes: check relative links/anchors, obsolete references
  and `git diff --check`; a full Rust rebuild is unnecessary.
- Shell changes: `bash -n` changed shell scripts; use ShellCheck if installed.
  Do not execute release scripts to validate them; use stubs for remote actions.
- Python compatibility changes: use Ruff on touched files if available and
  run the relevant scenarios. Syntax checking alone is not functional coverage.
- Protocol changes: consult `tests/compat/README.md`; external tools need
  checkouts, Go, Node/Chromium and local services. Report any checks not run.
- Native/dependency/release changes: verify GNU and musl builds. The release
  builder is `scripts/build-release.sh`; it checks musl ELF static linkage.
  Set `CC_x86_64_unknown_linux_musl=musl-gcc` and
  `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc` as in CI when needed.
- Dependency review: run cargo audit, `cargo deny --all-features --locked check`
  and `cargo machete --with-metadata` when available. The full deny policy
  passes after STEP-030 replaced chrono with Jiff. Do not suppress advisories
  to bypass future failures. See
  [dependency maintenance](doc/audit/dependencies.md) for constraints and results.

CI currently has separate manually dispatched Tests and Build and Release
workflows. Do not assume that publishing implies tests passed. Consult the
audit's release findings before using the local release helpers.

## Repository skills

- **commit** — [.agents/skills/commit/SKILL.md](.agents/skills/commit/SKILL.md): inspect changes,
  run relevant checks, create a detailed commit without AI attribution and
  push when authorized. Read this skill for commit requests; invoking `$commit`
  requests its commit-and-push workflow unless the user specifies local-only.
  Creating or editing the skill itself does not request a commit.
