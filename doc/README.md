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

- [Parser/resource stress checks](audit/resource-soaks.md): deterministic properties,
  mutation fuzzing and timed resource measurements.
- [Resource limits](audit/resource-limits.md): memory-history budgets, eviction and cursor lifetimes.

- [Message storage and indexes](message-text-index.md): current schema,
  migrations, history queries and maintenance.
- [Performance measurement](performance-layout.md): reader and storage
  benchmarks, memory layout and measurement limits.
- [Compatibility tests](../tests/compat/README.md): IRCv3, Go soju, listener
  plumbing and gamja checks.

## Development and audit

- [Protocol handler ownership](audit/protocol-handlers.md): typed history requests,
  command boundaries, service scope and ordered configuration validation.

- [Dependency maintenance](audit/dependencies.md): completed upgrades, selected
  features, dependency policy and remaining advisories/parent constraints.
- [Audit summary](audit_results.md): why/what/how, verified outcomes and remaining scope.
- [Audit records](audit/README.md): findings, authoritative checklist, verification,
  progress, source prompt and supporting assessments.

## Historical records

[The archive](archive/README.md) preserves earlier audits and benchmark results.
Use the current guides above for operational instructions; archived measurements
describe their original compiler, schema and workload.
