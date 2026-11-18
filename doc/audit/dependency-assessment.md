# Original dependency upgrade assessment

[Audit summary](../audit_results.md) · [Record index](README.md)

## 5. Dependency Upgrade Assessment

The completed upgrade/feature review and all 34 retained parent-version
constraints are documented in [dependency maintenance](dependencies.md).
The original dated inventory below remains for comparison.

STEP-006 migration review: rusqlite 0.43.0 changes empty/comment-only statement
handling and fixes optional APIs described in its upstream release notes.
No application API adaptation is required. Bundled `libsqlite3-sys` remains
0.40.0 with SQLite 3.54.1; schema 35, native configuration and database format
are unchanged. A new file-backed schema-30 migration/reopen regression verifies
history and cursor preservation, non-reused IDs, SQLite/foreign-key integrity
and FTS external-content integrity. All existing tests remain present.

Implementation follow-up (STEP-005): a live `cargo update --dry-run` refreshed
the index and identified seven compatible stable updates, now applied for
verification: bumpalo 3.20.4, cc 1.11.2, errno 0.3.15, mio 1.2.5,
rand 0.10.4, rustls-webpki 0.103.17 and zeroize 1.9.2. The tables below retain
the original audit snapshot; [§15](progress.md#15-implementation-progress-log) records verified implementation outcomes.
The rusqlite 0.43 migration is deliberately isolated in STEP-006.

The first live RustSec scan (cargo-audit 0.23.0, database commit
`233155b4025b854c68a4b199c0e32d7fd53291a2`, 1,527 advisories) reports zero
known vulnerabilities and the informational unmaintained-chrono advisory
`RUSTSEC-2026-0543`. No advisory was suppressed. STEP-007 assessed the migration
needed to replace chrono; a passing audit exit code does not mean no warnings.
STEP-030 subsequently replaces it with Jiff 1.0.0; the refreshed scan has no
vulnerabilities or advisory warnings and the full deny policy passes.

All 35 direct dependencies and every registry package name in Cargo.lock were queried against the live crates.io API during this run; no API query errors remained. Stable, non-yanked versions are the comparison target. These results are a dated inventory, **not proof that each latest transitive can satisfy its parents**. No upgrade was applied during the original audit; implementation updates are recorded above.

Source for each row: `https://crates.io/api/v1/crates/<crate>` (linked below). The full raw query snapshot is temporarily in `/tmp/soju-audit-deps.json`; this report preserves actionable version differences so that implementation does not depend on that temporary file.

| Direct crate | Declared / locked | Latest stable (2026-11-17) | Assessment |
| --- | --- | --- | --- |
| [aes-gcm](https://crates.io/api/v1/crates/aes-gcm) | 0.11.1 / 0.11.1 | 0.11.1 | Current |
| [anyhow](https://crates.io/api/v1/crates/anyhow) | 1.0.104 / 1.0.104 | 1.0.104 | Current |
| [aws-lc-rs](https://crates.io/api/v1/crates/aws-lc-rs) | 1.19.0 / 1.19.0 | 1.19.0 | Current |
| [base64](https://crates.io/api/v1/crates/base64) | 0.23.1 / 0.23.1 | 0.23.1 | Current |
| [bcrypt](https://crates.io/api/v1/crates/bcrypt) | 0.19.3 / 0.19.3 | 0.19.3 | Current |
| [chrono](https://crates.io/api/v1/crates/chrono) | 0.4.45 / 0.4.45 | 0.4.45 | Current |
| [futures-util](https://crates.io/api/v1/crates/futures-util) | 0.3.34 / 0.3.34 | 0.3.34 | Current |
| [gethostname](https://crates.io/api/v1/crates/gethostname) | 1.1.0 / 1.1.0 | 1.1.0 | Current |
| [hex](https://crates.io/api/v1/crates/hex) | 0.4.3 / 0.4.3 | 0.4.3 | Current |
| [hkdf](https://crates.io/api/v1/crates/hkdf) | 0.13.0 / 0.13.0 | 0.13.0 | Current |
| [http-body-util](https://crates.io/api/v1/crates/http-body-util) | 0.1.5 / 0.1.5 | 0.1.5 | Current |
| [hyper](https://crates.io/api/v1/crates/hyper) | 1.12.0 / 1.12.0 | 1.12.0 | Current |
| [hyper-util](https://crates.io/api/v1/crates/hyper-util) | 0.1.21 / 0.1.21 | 0.1.21 | Current |
| [indexmap](https://crates.io/api/v1/crates/indexmap) | 2.14.2 / 2.14.2 | 2.14.2 | Current |
| [ipnet](https://crates.io/api/v1/crates/ipnet) | 2.12.2 / 2.12.2 | 2.12.2 | Current |
| [libc](https://crates.io/api/v1/crates/libc) | 0.2.191 / 0.2.191 | 0.2.191 | Current |
| [mimalloc](https://crates.io/api/v1/crates/mimalloc) | 0.1.52 / 0.1.52 | 0.1.52 | Current |
| [p256](https://crates.io/api/v1/crates/p256) | 0.14.0 / 0.14.0 | 0.14.0 | Current |
| [rand](https://crates.io/api/v1/crates/rand) | 0.10.3 / 0.10.3 | 0.10.4 | Patch upgrade candidate |
| [rcgen](https://crates.io/api/v1/crates/rcgen) | 0.14.10 / 0.14.10 | 0.14.10 | Current |
| [reqwest](https://crates.io/api/v1/crates/reqwest) | 0.13.5 / 0.13.5 | 0.13.5 | Current |
| [rusqlite](https://crates.io/api/v1/crates/rusqlite) | 0.42.0 / 0.42.0 | 0.43.0 | Semver-breaking pre-1.0 upgrade; dedicated migration |
| [rustls](https://crates.io/api/v1/crates/rustls) | 0.23.46 / 0.23.46 | 0.23.46 | Current |
| [rustls-native-certs](https://crates.io/api/v1/crates/rustls-native-certs) | 0.8.5 / 0.8.5 | 0.8.5 | Current |
| [serde](https://crates.io/api/v1/crates/serde) | 1.0.229 / 1.0.229 | 1.0.229 | Current |
| [serde_json](https://crates.io/api/v1/crates/serde_json) | 1.0.154 / 1.0.154 | 1.0.154 | Current |
| [sha1](https://crates.io/api/v1/crates/sha1) | 0.11.0 / 0.11.0 | 0.11.0 | Current |
| [sha2](https://crates.io/api/v1/crates/sha2) | 0.11.1 / 0.11.1 | 0.11.1 | Current |
| [socket2](https://crates.io/api/v1/crates/socket2) | 0.6.5 / 0.6.5 | 0.6.5 | Current |
| [time](https://crates.io/api/v1/crates/time) | 0.3.55 / 0.3.55 | 0.3.55 | Current |
| [tokio](https://crates.io/api/v1/crates/tokio) | 1.54.1 / 1.54.1 | 1.54.1 | Current |
| [tokio-rustls](https://crates.io/api/v1/crates/tokio-rustls) | 0.26.6 / 0.26.6 | 0.26.6 | Current |
| [tokio-tungstenite](https://crates.io/api/v1/crates/tokio-tungstenite) | 0.30.0 / 0.30.0 | 0.30.0 | Current |
| [tokio-util](https://crates.io/api/v1/crates/tokio-util) | 0.7.20 / 0.7.20 | 0.7.20 | Current |
| [webpki-roots](https://crates.io/api/v1/crates/webpki-roots) | 1.0.12 / 1.0.12 | 1.0.12 | Current |

`rusqlite` 0.42 → 0.43 is a **semver-breaking pre-1.0 minor** migration, equivalent to a major compatibility review, not a patch-only update. Its [official 0.43 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.43.0) changes empty/comment-only SQL handling and fixes issues in changeset streaming, column metadata, virtual-table value handling and time conversion. The resolved features here are `bundled`, `cache`, default/cache support, `modern_sqlite` and the wasm binding support feature; session, column_metadata, vtab and time are not enabled. The cited fixes therefore do not by themselves establish a reachable vulnerability in soju-rs. Recheck feature reachability with advisory tooling. Version 0.43 declares Rust 1.89; rand 0.10.4 declares 1.85, both within the installed compiler's support.

All remaining locked-to-latest differences (excluding direct crates already above):

| Locked registry package | Locked | Latest stable | Assessment |
| --- | --- | --- | --- |
| [asn1-rs-derive](https://crates.io/api/v1/crates/asn1-rs-derive) | 0.6.0 | 0.7.0 | Review parent/target constraint before upgrade |
| [asn1-rs-impl](https://crates.io/api/v1/crates/asn1-rs-impl) | 0.2.0 | 0.3.0 | Review parent/target constraint before upgrade |
| [bit-vec](https://crates.io/api/v1/crates/bit-vec) | 0.9.1 | 0.10.2 | Review parent/target constraint before upgrade |
| [bumpalo](https://crates.io/api/v1/crates/bumpalo) | 3.20.3 | 3.20.4 | Patch candidate; refresh index and resolve |
| [cc](https://crates.io/api/v1/crates/cc) | 1.11.1 | 1.11.2 | Patch candidate; refresh index and resolve |
| [errno](https://crates.io/api/v1/crates/errno) | 0.3.14 | 0.3.15 | Patch candidate; refresh index and resolve |
| [getrandom](https://crates.io/api/v1/crates/getrandom) | 0.2.17 | 0.4.3 | Review parent/target constraint before upgrade |
| [lock_api](https://crates.io/api/v1/crates/lock_api) | 0.4.14 | 0.5.0 | Review parent/target constraint before upgrade |
| [mio](https://crates.io/api/v1/crates/mio) | 1.2.4 | 1.2.5 | Patch candidate; refresh index and resolve |
| [nom](https://crates.io/api/v1/crates/nom) | 7.1.3 | 8.0.0 | Review parent/target constraint before upgrade |
| [num-bigint](https://crates.io/api/v1/crates/num-bigint) | 0.4.8 | 0.5.1 | Review parent/target constraint before upgrade |
| [parking_lot](https://crates.io/api/v1/crates/parking_lot) | 0.12.5 | 0.13.0 | Review parent/target constraint before upgrade |
| [parking_lot_core](https://crates.io/api/v1/crates/parking_lot_core) | 0.9.12 | 0.10.0 | Review parent/target constraint before upgrade |
| [quinn-udp](https://crates.io/api/v1/crates/quinn-udp) | 0.5.16 | 0.6.3 | Review parent/target constraint before upgrade |
| [r-efi](https://crates.io/api/v1/crates/r-efi) | 6.0.0 | 7.2.0 | Platform-specific; resolve through parent |
| [redox_syscall](https://crates.io/api/v1/crates/redox_syscall) | 0.5.18 | 0.9.5 | Platform-specific; resolve through parent |
| [rusticata-macros](https://crates.io/api/v1/crates/rusticata-macros) | 4.1.0 | 5.0.0 | Review parent/target constraint before upgrade |
| [rustls-webpki](https://crates.io/api/v1/crates/rustls-webpki) | 0.103.16 | 0.103.17 | Patch candidate; refresh index and resolve |
| [syn](https://crates.io/api/v1/crates/syn) | 2.0.119 | 3.0.10 | Review parent/target constraint before upgrade |
| [synstructure](https://crates.io/api/v1/crates/synstructure) | 0.13.2 | 0.14.0 | Review parent/target constraint before upgrade |
| [tower-http](https://crates.io/api/v1/crates/tower-http) | 0.6.11 | 0.7.1 | Review parent/target constraint before upgrade |
| [untrusted](https://crates.io/api/v1/crates/untrusted) | 0.7.1 | 0.9.0 | Review parent/target constraint before upgrade |
| [wasi](https://crates.io/api/v1/crates/wasi) | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | Platform-specific; resolve through parent |
| [windows-core](https://crates.io/api/v1/crates/windows-core) | 0.62.2 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-implement](https://crates.io/api/v1/crates/windows-implement) | 0.60.2 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-interface](https://crates.io/api/v1/crates/windows-interface) | 0.59.3 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-link](https://crates.io/api/v1/crates/windows-link) | 0.2.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-result](https://crates.io/api/v1/crates/windows-result) | 0.4.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-strings](https://crates.io/api/v1/crates/windows-strings) | 0.5.1 | 0.100.0 | Platform-specific; resolve through parent |
| [windows-sys](https://crates.io/api/v1/crates/windows-sys) | 0.52.0 | 0.61.2 | Platform-specific; resolve through parent |
| [windows-targets](https://crates.io/api/v1/crates/windows-targets) | 0.52.6 | 0.53.5 | Platform-specific; resolve through parent |
| [windows_aarch64_gnullvm](https://crates.io/api/v1/crates/windows_aarch64_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_aarch64_msvc](https://crates.io/api/v1/crates/windows_aarch64_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_gnu](https://crates.io/api/v1/crates/windows_i686_gnu) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_gnullvm](https://crates.io/api/v1/crates/windows_i686_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_i686_msvc](https://crates.io/api/v1/crates/windows_i686_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_gnu](https://crates.io/api/v1/crates/windows_x86_64_gnu) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_gnullvm](https://crates.io/api/v1/crates/windows_x86_64_gnullvm) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [windows_x86_64_msvc](https://crates.io/api/v1/crates/windows_x86_64_msvc) | 0.52.6 | 0.53.1 | Platform-specific; resolve through parent |
| [zeroize](https://crates.io/api/v1/crates/zeroize) | 1.9.1 | 1.9.2 | Patch candidate; refresh index and resolve |

Compatible candidates include bumpalo, cc, errno, mio, rustls-webpki and zeroize patch updates. Parent-constrained breaking lines include parking_lot/lock_api, tower-http, nom, syn, ASN.1/Windows/WASI dependencies and older getrandom/untrusted. Some entries are platform-only or optional rather than part of the tested Linux feature graph. Upgrade parents where supported, or document the constraint; do not require every package to share its global latest release. Default active-target `cargo tree --duplicates` reports only `untrusted` 0.7.1 (AWS-LC) and 0.9.0 (webpki); neither duplication alone establishes waste or a vulnerability.

`cargo update --dry-run --offline` found zero updates using the **cached index**. This does not contradict the live API: it shows why a refreshed index is required before the implementation refresh. Neither Cargo.lock nor manifests changed during that original dry run. At audit time `cargo outdated`, `cargo audit`, `cargo deny`, and `cargo machete` were unavailable. Current implementation results are in [§12](verification.md#12-verification-matrix): audit/deny/machete were subsequently installed; cargo-outdated remains absent. No memory-safety proof or advisory suppression is claimed. Direct source usage was reviewed; no dependency was proven safely removable. Tokio's broad `full` feature merits a later feature audit, not an untested feature reduction.

Native assessment: application code has no unsafe/FFI calls to justify bypassing safety checks. Native code remains in SQLite, AWS-LC and mimalloc. Both deployment targets must retain native build/static-link checks. Keep the current allocator until representative memory measurements justify a change. Dependency freshness is not a substitute for an advisory/native security review.
