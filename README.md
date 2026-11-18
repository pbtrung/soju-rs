# soju-rs

A Rust port of [soju], a multi-user IRC bouncer. It stays connected to upstream
IRC servers while your clients are offline, stores chat history and supports
multiple networks, detached channels and IRCv3 extensions.

soju-rs targets upstream soju v0.11 configuration and protocol compatibility,
with documented differences and Rust extensions for multiplexing and unread
state. The local Gamja and Senpai forks pass the recorded client checks;
exact Go transcript equivalence is not established. See
[compatibility](doc/compatibility.md) for supported behavior and verification limits.

## Usage

Start with [getting started](doc/getting-started.md) for a complete configuration,
TLS, service setup, accounts and client connections. The server must be running
with an admin socket before `sojuctl` can manage users:

```sh
soju -config /etc/soju/config
# In another terminal, using an account name and password you choose:
sojuctl -config /etc/soju/config user create -username alice -password '<password>' -admin
```

The SQLite database holds credentials, keys and history. Keep its directory
private and back it up before upgrading. The current schema is 35; older
soju-rs and upstream soju cannot open an upgraded database. Read the
[upgrade instructions](doc/message-text-index.md#upgrades-and-compatibility)
before migrating an existing installation.

## Building

Use Rust 1.100.0 or newer (Edition 2024). Distro Rust and rustup are both
supported. Native SQLite, AWS-LC and mimalloc builds require a C compiler,
CMake, Make and Perl.

```sh
cargo build --release --locked
```

The executables are `target/release/soju` and `target/release/sojuctl`.
[Release archives](https://github.com/pbtrung/soju-rs/releases) target Linux
x86_64 GNU and static musl. For local archive builds, checksums, GitHub workflows
and release helpers, see [building and releasing](doc/releasing.md).

### Arch Linux dependencies

For development, GNU builds and the Rust test suite:

```sh
sudo pacman -Syu --needed base-devel rust git cmake perl
```

[base-devel](https://archlinux.org/packages/core/any/base-devel/) supplies GCC,
Make and binutils. Arch's [rust package](https://archlinux.org/packages/extra/x86_64/rust/files/)
includes Cargo, rustfmt and Clippy; check `rustc --version` against the minimum
above. Cargo fetches the Rust dependencies and builds bundled SQLite, AWS-LC
and mimalloc.

For x86_64 musl builds and tests, also install:

```sh
sudo pacman -Syu --needed musl rust-musl
```

For the external Go/IRCv3/transport/Gamja checks and SQLite benchmarks:

```sh
sudo pacman -Syu --needed go python nodejs npm chromium openssl iproute2 procps-ng sqlite
```

Go builds the reference bouncer and Ergo; Node.js/npm build Gamja; Chromium runs
browser tests. OpenSSL generates test certificates, `iproute2` supplies `ss`,
and `procps-ng` supplies `pgrep` for transport checks. The `sqlite` shell is for
storage benchmarks, not a prerequisite for the bundled database build.
Use `CHROMIUM_EXECUTABLE=/usr/bin/chromium` with the
[compatibility runner](tests/compat/README.md#test-a-local-gamja-fork).

Optional development tools are `rust-analyzer` (editor integration), `ruff`
(Python formatting/lint), `shellcheck` (shell lint), `scdoc` (manual rendering)
and `github-cli` (release helpers). Install only what your task needs; see
[development checks](doc/development.md#required-checks) and
[release prerequisites](doc/releasing.md).

## Differences from upstream soju

Rust-specific capabilities provide [multiplexing](doc/multiplex.md) and
[unread/mention snapshots](doc/unread.md). Notable unsupported upstream features
include file uploads, PostgreSQL, PAM and the filesystem message store.
Timestamp handling, memory retention, transport behavior and storage layout
also have intentional differences. The complete list is in
[compatibility](doc/compatibility.md#differences-from-upstream-soju).

## Testing

[Development](doc/development.md) describes source ownership, required checks
and GNU/musl verification. Server integration scenarios run on both Tokio
runtime flavors; blocking actor work is admitted through a bounded worker
boundary to keep independent networking responsive during SQLite contention.

[Compatibility tools](tests/compat/README.md) test IRCv3, transports, Go soju
transcripts and Gamja browser flows. Senpai's real-bouncer integration runs
from its own checkout. [Parser mutation campaigns](tests/fuzz/README.md),
[resource limits](doc/resource-limits.md) and
[performance measurements](doc/performance-layout.md) cover other focused checks.

The **Tests** and **Build and Release** GitHub workflows are manually dispatched.
Publishing does not require a successful Tests run; run tests against the same
commit before publishing. [Audit records](doc/audit_results.md) preserve actual
results, unresolved CI findings and verification limits.

## Documentation

The [documentation index](doc/README.md) groups operator, client and developer
guides. Command and configuration references are in the
[server manual](doc/soju.1.scd) and [admin-client manual](doc/sojuctl.1.scd).

## License

AGPL-3.0-only; see [LICENSE](LICENSE).

Copyright (C) 2020 The soju Contributors

[soju]: https://soju.im
