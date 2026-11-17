# Documentation

## Running soju-rs

- [Getting started](getting-started.md): TLS, service setup, accounts and clients.
- [Server manual](soju.1.scd): configuration and BouncerServ commands.
- [Administration manual](sojuctl.1.scd): using the local admin socket.

For installation, supported builds and differences from upstream, see the
[project README](../README.md).

## Protocol extensions

- [Multiplexing](multiplex.md): carry multiple networks on one connection.
- [Unread and mention state](unread.md): tags on `CHATHISTORY TARGETS` replies.

## Storage and performance

- [Message storage and indexes](message-text-index.md): current schema,
  migrations, history queries and maintenance.
- [Performance measurement](performance-layout.md): reader and storage
  benchmarks, memory layout and measurement limits.
- [Compatibility tests](../tests/compat/README.md): IRCv3, Go soju, listener
  plumbing and gamja checks.

## Audit and implementation planning

- [Audit results and implementation checklist](audit_results.md): findings,
  verification evidence and planned work from 2026-11-17. Planned fixes remain
  unimplemented unless their status explicitly says otherwise.
- [Audit prompt](audit_prompt.md): scope, reporting requirements and the
  audit-only instruction used for that run.

## Historical records

[The archive](archive/README.md) preserves earlier audits and benchmark results.
Use the current guides above for operational instructions; archived measurements
describe their original compiler, schema and workload.
