# Documentation

Choose a guide for the task you are doing. The manuals describe accepted
commands and configuration; the audit records describe dated verification.

## Run and administer a bouncer

| Guide | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Complete configuration, TLS, systemd, accounts and clients |
| [TLS troubleshooting](tls-troubleshooting.md) | Upstream handshake failures and the P2P-NET server-selection workaround |
| [Server manual](soju.1.scd) | Configuration directives and BouncerServ commands |
| [Administration manual](sojuctl.1.scd) | Admin socket and `sojuctl` |
| [Resource limits](resource-limits.md) | Admission, history/cache limits, overload and shutdown behavior |
| [Message storage](message-text-index.md) | Schema 35, backup/migration, history indexes and maintenance |
| [Building and releasing](releasing.md) | GNU/musl archives, checksums, workflows and release helpers |

## Connect or implement a client

- [Compatibility](compatibility.md): upstream differences, supported client behavior
  and the limits of recorded test results.
- [Multiplexing](multiplex.md): several networks on one physical connection.
- [Unread and mention state](unread.md): snapshot tags on `CHATHISTORY TARGETS`.
- [Compatibility tests](../tests/compat/README.md): IRCv3, transport, Go comparison,
  local Gamja and Senpai checks.

## Develop and verify changes

- [Development](development.md): source map, ownership rules and required checks.
- [Protocol handler ownership](audit/protocol-handlers.md): typed history requests,
  service scope, command dispatch and configuration validation.
- [Dependency maintenance](audit/dependencies.md): selected features, timestamp
  migration, dependency policy and the dated version assessment.
- [Performance measurement](performance-layout.md): reader/storage benchmarks,
  actor dispatch, memory layout and resource soaks.
- [Parser mutation campaigns](../tests/fuzz/README.md): bounded inputs, seeds,
  replay and failure reduction.

## Read the verification record

[Audit summary](audit_results.md) is the entry point. The
[audit index](audit/README.md) separates findings, the implementation checklist,
progress, baseline and final results. The
[final assessment](audit/final-verification.md) records the 2026-11-18 client,
deployment, performance and resource checks, including failures and limits.

The [archive](archive/README.md) preserves earlier investigations. Its commands,
measurements and proposed optimizations apply to the versions stated there;
use the maintained guides above for current operating instructions.

[Project README](../README.md)
