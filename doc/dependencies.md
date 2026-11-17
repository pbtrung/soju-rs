# Dependency maintenance

Reviewed on 2026-11-17 with Rust 1.100.0. The package uses 35 direct
dependencies; the reduced lockfile contains 268 packages including soju.
The original inventory remains in [the audit report](audit_results.md#5-dependency-upgrade-assessment).

## Completed updates

| Dependency | Previous | Current |
| --- | --- | --- |
| rand | 0.10.3 | 0.10.4 |
| rusqlite | 0.42.0 | 0.43.0 |
| bumpalo | 3.20.3 | 3.20.4 |
| cc | 1.11.1 | 1.11.2 |
| errno | 0.3.14 | 0.3.15 |
| mio | 1.2.4 | 1.2.5 |
| rustls-webpki | 0.103.16 | 0.103.17 |
| zeroize | 1.9.1 | 1.9.2 |

The [rusqlite release](https://github.com/rusqlite/rusqlite/releases/tag/v0.43.0)
changes empty/comment-only statement handling and fixes optional APIs. Our
queries require no API adaptation. Bundled libsqlite3-sys remains 0.40.0
(SQLite 3.54.1); schema 35 and persisted message IDs are unchanged. The new
[file-backed regression](../src/database/sqlite/tests/migrations.rs) checks
migration/reopen behavior, FTS, foreign keys, delivery cursors and ID allocation.

## Selected features

Tokio explicitly enables filesystem, buffered I/O, macros, networking, runtime,
signals, synchronization and timers. Unused process/stdio support is removed.
The existing parking_lot locking backend is retained; the dev dependency adds
only test-util to the production feature set.

rusqlite explicitly enables bundled native SQLite and statement caching.
Disabling its default WASM backend removes sqlite-wasm-rs and rsqlite-vfs
from the lockfile. Required GNU and musl targets retain their native backend;
this selection does not promise WASM support. Existing reqwest and rcgen
choices already disable unnecessary defaults and are retained.

cargo-machete found no unused direct dependencies; none were removed based
only on a textual search. Parent-owned major-version constraints are retained
rather than forced through incompatible patches.

## Advisory and license policy

[deny.toml](../deny.toml) rejects yanked crates and unreviewed Git/registry
sources. Its license list covers this AGPL project and the SPDX choices
present in the inspected dependency graph, including build/dev dependencies.
Duplicate major versions remain visible warnings; they are not suppressed.
No advisory IDs are ignored.

**Outstanding:** RustSec RUSTSEC-2026-0543 identifies chrono as unmaintained,
with no patched chrono release. cargo-audit reports zero known vulnerabilities
and this informational warning; the full cargo-deny check **fails** on the
maintenance advisory. License, source and duplicate-policy checks pass.
[DEPS-002 / STEP-030](audit_results.md#deps-002--replace-unmaintained-chrono-with-jiff)
tracks the user-requested replacement with jiff. Timestamp/date-range,
fractional-second, local-time and database compatibility need verification
before removing chrono. This follow-up is planned, not implemented.

Checks used cargo-audit 0.23.0, cargo-deny 0.21.0 and cargo-machete 0.10.1.
Tools were installed into a temporary directory for this run; they are not
repository dependencies. With those tools installed on PATH, repeat:

```sh
cargo update --dry-run
cargo tree --locked --target all -e features
cargo audit --json --file Cargo.lock
cargo deny --all-features --locked check
cargo machete --with-metadata
```

A successful cargo-audit exit code does not mean that its warning list is
empty. The full cargo-deny command above is expected to remain failing until
STEP-030 resolves the chrono advisory; do not add an ignore just to pass it.

## Remaining parent-constrained versions

A refreshed registry dry run finds no remaining compatible updates. The
following locked versions differ from the latest stable releases in the
live crates.io inventory. The parent requirements explain why ordinary
resolution cannot advance them. Some parents are optional or platform-specific;
GNU/musl verification does not establish support for other targets. Revisit
these rows when parents publish compatible updates.

| Package | Locked | Latest stable | Parent requirements |
| --- | --- | --- | --- |
| asn1-rs-derive | 0.6.0 | 0.7.0 | asn1-rs 0.7.2 (^0.6) |
| asn1-rs-impl | 0.2.0 | 0.3.0 | asn1-rs 0.7.2 (^0.2) |
| bit-vec | 0.9.1 | 0.10.2 | yasna 0.6.0 (^0.9.1) |
| getrandom | 0.2.17 | 0.4.3 | ring 0.17.14 (^0.2.10) |
| lock_api | 0.4.14 | 0.5.0 | parking_lot 0.12.5 (^0.4.14) |
| nom | 7.1.3 | 8.0.0 | asn1-rs 0.7.2 (^7.0); der-parser 10.0.0 (^7.0); rusticata-macros 4.1.0 (^7.0); x509-parser 0.18.1 (^7.0) |
| num-bigint | 0.4.8 | 0.5.1 | der-parser 10.0.0 (^0.4) |
| parking_lot | 0.12.5 | 0.13.0 | tokio 1.54.1 (^0.12.0) |
| parking_lot_core | 0.9.12 | 0.10.0 | parking_lot 0.12.5 (^0.9.12) |
| quinn-udp | 0.5.16 | 0.6.3 | quinn 0.11.12 (^0.5) |
| r-efi | 6.0.0 | 7.2.0 | getrandom 0.4.3 (^6) |
| redox_syscall | 0.5.18 | 0.9.5 | parking_lot_core 0.9.12 (^0.5) |
| rusticata-macros | 4.1.0 | 5.0.0 | asn1-rs 0.7.2 (^4.0); der-parser 10.0.0 (^4.0); x509-parser 0.18.1 (^4.0) |
| syn | 2.0.119 | 3.0.10 | asn1-rs-derive 0.6.0 (^2.0); asn1-rs-impl 0.2.0 (^2.0); jni-macros 0.22.4 (^2.0); jni-sys-macros 0.4.1 (^2); synstructure 0.13.2 (^2); windows-implement 0.60.2 (^2.0); windows-interface 0.59.3 (^2.0) |
| synstructure | 0.13.2 | 0.14.0 | asn1-rs-derive 0.6.0 (^0.13) |
| tower-http | 0.6.11 | 0.7.1 | reqwest 0.13.5 (^0.6.8) |
| untrusted | 0.7.1 | 0.9.0 | aws-lc-rs 1.19.0 (^0.7.1) |
| wasi | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | getrandom 0.2.17 (^0.11); mio 1.2.5 (^0.11.0) |
| windows-core | 0.62.2 | 0.100.0 | iana-time-zone 0.1.65 (>=0.56, <=0.62) |
| windows-implement | 0.60.2 | 0.100.0 | windows-core 0.62.2 (^0.60.2) |
| windows-interface | 0.59.3 | 0.100.0 | windows-core 0.62.2 (^0.59.3) |
| windows-link | 0.2.1 | 0.100.0 | chrono 0.4.45 (^0.2); gethostname 1.1.0 (^0.2.1); jni 0.22.4 (^0.2); parking_lot_core 0.9.12 (^0.2.0); windows-core 0.62.2 (^0.2.1); windows-result 0.4.1 (^0.2.1); windows-strings 0.5.1 (^0.2.1); windows-sys 0.61.2 (^0.2.1) |
| windows-result | 0.4.1 | 0.100.0 | windows-core 0.62.2 (^0.4.1) |
| windows-strings | 0.5.1 | 0.100.0 | windows-core 0.62.2 (^0.5.1) |
| windows-sys | 0.52.0 | 0.61.2 | ring 0.17.14 (^0.52) |
| windows-targets | 0.52.6 | 0.53.5 | windows-sys 0.52.0 (^0.52.0) |
| windows_aarch64_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_aarch64_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_gnu | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_gnu | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |

[Documentation index](README.md)
