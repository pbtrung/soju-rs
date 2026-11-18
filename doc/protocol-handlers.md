# Protocol handler ownership

Mutable user and network state belongs to the user actor. Each event completes
before the next event runs. Handlers execute through the bounded blocking
boundary described in [resource limits](resource-limits.md#actor-database-work-and-maintenance).
The Phase 7 refactor preserves this ownership and existing wire behavior.

## History requests

`src/downstream/cmds/history.rs` separates four responsibilities:

- `HistoryFields` identifies the command and borrows its wire fields. The
  entry point retains queue admission and the existing service/bouncer shortcuts.
- Validation produces a `HistoryRequest` with typed timestamps, a validated
  limit and an exhaustive `HistoryQuery` variant. Bounds and limits are parsed
  once, after the shortcuts that historically precede their validation.
- `HistoryQuery::execute` performs store queries without downstream state.
  AROUND retains its before/after split and fallback bound; BETWEEN retains
  direction and exclusive timestamp bounds. LATEST has no implicit upper bound,
  so upstream clocks ahead of the bouncer still work.
- Message and TARGETS replies keep their batch types, canonical target,
  detached-channel filtering and optional unread/mention tags.

READ/MARKREAD remains separate because its persistence and broadcast policy is
independent of history request parsing. Search and the store's ordering/tie
rules retain their existing behavior.

## Upstream and downstream commands

Upstream modules keep the existing dispatch order: messages, registration,
users, then channels. Each module delegates substantial command branches to
named handlers; small cohesive branches stay in their dispatcher. Chat routing
is separate from detached delivery, mention persistence and push decisions.

`upstream::connection` returns a fallible, scoped borrow of actor-owned
connection state. Dispatch, extracted command handlers and CAP acknowledgments
use it instead of assuming optional state exists. The event boundary still
rejects stale connection generations. Label/batch setup and cleanup remain
around the complete command dispatch, including error results.

Downstream registration separates SASL completion and BOUNCER binding from its
command dispatcher. CAP REQ validates all changes before applying the set.
Welcome sends identity/ISUPPORT, initial state, joined channels and backlog in
their established order. None of these operations introduces concurrent state
mutation or a new dynamic dispatch layer.

## Services and configuration

`src/service/registry.rs` holds command families, help/abbreviation lookup and
callback types. A user-scoped callback accepts `&mut User`; global callbacks
accept an optional caller because admin-socket commands have none. Dispatch
checks authorization and user scope before invoking a callback. User handlers
no longer unwrap optional state, and no synthetic user is substituted.

Registry declarations retain command names, usage/help text, administrative
requirements and global flags. Global user operations retain their self-user
and deferred cross-user execution paths.

`src/config/directives.rs` owns directive arity and driver validation. Parsing
calls identity, storage, authentication, HTTP, connection-policy and lifecycle
groups in the original order. Unknown directives are rejected before those
groups; duplicate/child/argument errors, defaults and legacy aliases retain
their existing precedence. Public configuration types remain in `config.rs`.
