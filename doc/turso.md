# Turso Cloud storage

`message-store turso` stores bouncer data in Turso Cloud. It creates a
controller database and one database per user in an existing Turso group.
It replaces the local `db` backend for the whole bouncer; omit the `db`
directive when using it.

The controller is the account directory: usernames, bcrypt password hashes,
account settings and permissions, authentication certificates, and the link
and provisioning state for each user database. It also keeps the bouncer's
Web Push signing identity. Networks, upstream credentials, channels, delivery
and read receipts, Web Push subscriptions, messages, and the full-text search
index live in the user's database. Tenant databases contain `TursoOwner`
with the controller name and owner ID; they have no `User` table or login
password hash. `Network.user` and `WebPushSubscription.user` reference
`TursoOwner.user`. Account authentication and certificates use the
controller. Local SQLite retains its combined account/data schema.
No local database file is needed for cloud storage.

## Configuration

Create a Turso organization and group first, then configure:

```scfg
listen irc+insecure://localhost:6667
listen unix+admin://
message-store turso
turso-organization my-organization
turso-group bouncer
turso-controller soju-controller
turso-group-token "GROUP_SQL_TOKEN"
turso-api-token "PLATFORM_API_TOKEN"
```

`turso-controller` is optional and defaults to `soju-controller`. Use a
different controller name and a separate group for each independent bouncer.
Run one soju instance per controller. Storage and credential configuration
changes require a restart. Changing `turso-controller` selects a different
account directory; it does not rename or migrate the existing databases.

There are two different kinds of Turso credential:

- `turso-group-token`: a full-access SQL token for every database in the
  configured group. For example, create one with
  `turso group tokens create bouncer --expiration never`.
- `turso-api-token`: a Platform API token with `read`, `db:create`, and
  `db:delete` permissions for the organization/group. Provisioning uses it to
  retrieve, create and delete databases. A group SQL token cannot perform
  these Platform API operations. See [Turso API authentication].

Keep the configuration readable only by the account running soju; it contains
both tokens. Tokens must remain valid for the bouncer to operate. Rotate them
in the configuration and restart soju. SQL uses HTTPS and Turso's
[SQL-over-HTTP protocol]. The client tracks replication indexes so subsequent
reads wait for its previous writes. Remote event handlers run on blocking
workers and independent HTTP streams run concurrently, including when soju
uses a single-threaded runtime. Cloud round-trip latency still affects the
user issuing a command; place soju near the group's primary region.

Writes are not automatically replayed after network errors. If the server
commits a write but its response is lost, the operation reports failure and
its outcome is uncertain. Check the saved state before repeating it. A failed
transaction is discarded instead of continuing on a new auto-commit stream.

Use v0.99.21 or later with hosted Turso. It closes a transaction's stream in
the same pipeline as COMMIT, accepting the null baton returned after a
successful commit. v0.99.20 could incorrectly report `Turso closed the stream
during a transaction` after creating the controller schema. Keep the same
controller and group when upgrading; the committed schema can be reopened.

## Create an administrator

Before starting soju, bootstrap an administrator and their database:

```sh
sojuctl -config /etc/soju/config turso create-admin -username alice -password secret
soju -config /etc/soju/config
```

This explicit bootstrap command connects directly to Turso and needs no
running server or admin socket. Run it while soju is stopped, or restart soju
afterwards to load the new user. It creates a new account and fails if the
username already exists.

With soju running, the existing admin-socket commands work too:

```sh
sojuctl -config /etc/soju/config user create -username bob -password secret -admin
```

Both paths provision the user's database before enabling sign-in. Clients
sign in with their bouncer username and password as usual.

## Provisioning and recovery

Startup creates the controller if it is missing, opens every linked user
database, and initializes or upgrades their schemas before accepting clients.
Each user database name is a fixed-width, 25-character base36 encoding of
128 cryptographically random bits, using only `a-z0-9`. Names are unrelated
to usernames and are retained across restarts.

Account creation first commits the account and database name to the
controller with a `provisioning` state. If database creation or schema setup
fails, sign-in stays disabled for that account. Fix the credentials or cloud
availability issue and restart soju to resume with the same database name.
Startup fails if any linked database cannot be initialized. Existing database
names must belong to the configured group. Once a database is marked `ready`,
startup refuses to recreate it if it disappears; restore the database and
restart. Only new accounts in `provisioning` state can create a missing user
database. Each user database also records its controller and owner to reject
links to another account's data.

Back up the controller together with the user databases. The controller's
links and password hashes are needed to recover the account directory; user
database names cannot reconstruct it. If the controller itself is missing,
startup creates an empty directory, as it does on the first run.

Deleting a user through BouncerServ deletes their cloud database too. The
controller records `deleting` first; interrupted deletions resume on startup
without recreating the database. Turso delete protection must be disabled for
user databases that should be removable through soju.
Deletion validates the link and group again before calling the Platform API;
it refuses to delete the controller or a database moved to another group.

## Upgrading to v0.99.23

Back up the controller and all user databases before upgrading. User
databases become schema 33 (local SQLite also records 33, without changes to
its tables). The controller stays at version 2. Earlier binaries cannot
reopen upgraded user databases.

Startup rebuilds each user's `Message` table into the clustered layout
described in [Message table layout](#message-table-layout). Rows are copied
in ID order, in transactions of at most 1,000 messages, committing a cursor
with each chunk. An interrupted or ambiguous copy resumes on restart without
duplicating rows; timeouts halve the chunk size. The old table, its three
indexes and its search triggers are replaced only after copying completes.
Message IDs are kept, so the search index and delivery receipts stay valid
without a rebuild. Message ID reservations continue from the highest ID
already stored.

The copy is a one-time cost of about one row read and one row written per
stored message, plus one written per text message for its ID index. In the
[real-engine measurement](turso-query-evidence.md), migrating 100,400 event
rows read 403,904 rows and wrote 100,906. Every message written afterwards
saves at least one row write, so the copy is repaid once as many new
messages have been stored. Allow startup time for large histories. Upgrading
from v0.99.21 or earlier skips creating the schema-32 event index on Turso,
since the rebuild replaces it.

## Upgrading to v0.99.22

Back up the controller and all user databases before upgrading. The controller
schema becomes version 2 and tenant/local schemas become version 32. Earlier
binaries cannot reopen the upgraded schemas; rollback requires compatible
backups of both the controller and its user databases.

Startup migrates existing tenants automatically. It validates ownership,
retargets the network/subscription foreign keys, and removes the old password-free
User anchor and unused authentication tables. Unexpected authentication records
in a tenant stop migration instead of being silently discarded. Do not manually
drop User or change database names.

The FTS replacement copies text rows in transactions of at most 200 rows,
committing a cursor with each chunk. It resumes after interruption or an
ambiguous response. Timeouts reduce the chunk size. The old search index is
replaced only after copying completes. Rebuilding indexes incurs one-time
reads/writes and can delay startup on large histories; allow sufficient startup
probe time. Messages and their IDs are retained. No VACUUM is run.

## Measuring row usage

The metrics endpoint includes `soju_turso_rows_read_total` and
`soju_turso_rows_written_total`, summed from received Hrana responses. They
include controller and tenant SQL and reset when soju restarts. Requests whose
responses are lost, errors without counters, and server-side/background work
cannot be reconstructed from these counters; they are not billing totals.

See the [query optimization audit](turso-query-optimization.md) and
[reproducible engine measurements](turso-query-evidence.md). Fewer HTTP requests
do not imply fewer row reads or writes: constraints, indexes, FTS, and retries
all matter. Resaving unchanged receipts and records now avoids redundant writes.

## Background message batches

Turso buffers incoming logical IRC messages, including logged events, per
user across their networks. An assembled multiline message counts as one. `chathistory-limit` is also the
batch line target (default 1000, range 100–1000); config reload updates the
target. A one-second timer flushes partial batches on quiet connections.
The filling buffer is also dispatched at an 8 MiB message-data budget.

Messages use multi-row VALUES inserts of up to 100 lines per SQL statement,
sharing one HTTP batch with one BEGIN and one COMMIT. Target IDs are resolved
once and kept in a bounded cache; foreign keys remain enabled. An identical
retry does not UPDATE the existing rows or repeat their FTS writes. A row the
database rejects with a constraint error, such as a reserved ID reused with
different contents, is isolated by splitting the batch: the other rows are
stored, and the rejected one is dropped and logged rather than blocking
every later message of that user. For example, 400 short
messages arriving together with `chathistory-limit 200` use two message
transactions. A quiet connection receiving one message per second or less
can produce one-message transactions because of the partial-flush timer.

One buffer accepts messages while an independent background writer commits
the other. Commits use conditional Hrana batches, including BEGIN and
COMMIT, so a statement error cannot leave a partial transaction. Large
batches are split to keep each serialized HTTP body at or below 512 KiB,
including JSON escaping, bound arguments, and protocol envelopes. HTTP 413
responses and processing timeouts cause further splitting; a timeout also
lowers subsequent request sizes, which double again after eight successful
requests. These are application budgets, not a claim
about a fixed hosted quota. The upstream libSQL server defaults to a
[five-second transaction timeout][libsql transaction timeout]; hosted
settings and proxy limits may differ. One logical buffer may therefore
require several independently atomic requests.

Live IRC processing continues during commits. History, search, target lists,
and delivery cursors include pending messages through an in-memory SQLite
index using the same filters and FTS tokenizer. Reads use a separate HTTP
stream, sharing the writer's replication watermark, and merge results
without duplicates. Normal database queries still wait for their own HTTP
responses; they do not wait for the background writer's client-side lock.

If both buffers fill, incoming message events retain their socket input
permits and wait for space; control commands can still run, unless the same
connection already has a deferred event: each connection's input stays in
order, so a client's PART or QUIT cannot overtake its pending PRIVMSG. PING
and PONG are always answered. This applies backpressure rather than keeping
an unlimited backlog. A single IRC event
that fans out to several channels finishes before further input is deferred.
A long cloud outage can eventually exhaust the socket input budget too.
Pending network deletions cannot recreate their old parent networks when a
buffer is written later.

Message IDs are reserved durably in advance, with the next range fetched
in the background. Restarts and expiry may leave ID gaps but cannot reuse
an ID already exposed to a client. Only buffered message writes are retried:
the same reserved ID and contents make a lost-response retry idempotent,
and an ID collision with different contents fails the entire request.
Unrelated SQL writes retain their previous non-retryable behavior.

Messages are forwarded before Turso confirms persistence. Buffers and their
history index are in memory, so a crash or forced shutdown can lose pending
lines. Graceful shutdown drains both buffers and retries failures up to
three times, then logs any remaining flush error. Cloud unavailability
prevents a durability guarantee during that interval. Local `message-store
db` keeps its immediate SQLite writes.

## Message table layout

User databases store `Message` as a `WITHOUT ROWID` table with the primary
key `(target, time, id)`, plus a unique `MessageTextIdIndex ON
Message(target, id) WHERE text IS NOT NULL`. Writing an event (JOIN, PART,
QUIT, MODE…) costs one row; a text message also writes its ID index entry
and its search entry. Measured on the real engine, 200 events write 200
rows instead of 600, and 200 text messages 600 instead of 800.

- History with event playback, the latest message of each target, expiry
  and network deletion seek the primary key directly.
- Search results and delivery cursors seek the text ID index. A delivery
  cursor is the ID of the last *text* message: backlog for clients without
  CHATHISTORY replays text messages only, so later events never affect it.
- Text-only history and text-only target lists step over the events stored
  between text messages. This costs extra row reads, never writes; clients
  with `draft/event-playback` don't incur it.
- Message IDs are allocated only from durable reservations in
  `MessageIdHighWater`; the clustered table is never scanned for `max(id)`.

Sparse FTS indexes text rows only, with `columnsize=0` because search does
not use token-count ranking. Turso creates these structures without ANALYZE
or VACUUM. Local SQLite keeps its upstream-compatible rowid table with
`MessageTextIndex` and `MessageEventIndex`.

## Schema versions and maintenance

Cloud schema versions live in `SojuSchemaVersion`, avoiding writes to
`PRAGMA user_version`, which [Turso Cloud documents as read-only][cloud
limitations]. Existing databases initialized by v0.99.19 import their legacy
pragma version on first open, then use the table for future upgrades.
Cloud databases first initialized by v0.99.20 need v0.99.20 or later: older
builds do not understand this schema-version table.

Expiry removes messages from each user database in turn and optimizes the
FTS index of those that deleted any. A failing database is reported without
stopping the others. It never sends
`VACUUM`, `PRAGMA wal_checkpoint`, `PRAGMA shrink_memory` or file-space probes
to Turso, including after the startup expiry pass. These operations are for
local SQLite; the hosted SQL interface rejects client-issued `VACUUM` and
WAL checkpoints. Turso migrations also omit the local-only `ANALYZE` step.
See the [server SQL allowlist] for these restrictions. Existing local SQLite
data is not imported automatically when switching to Turso.

The pre-release [Turso audit](turso-audit.md) records the fault cases covered
by regression tests and the remaining verification limits.

[Turso API authentication]: https://docs.turso.tech/api-reference/authentication
[SQL-over-HTTP protocol]: https://docs.turso.tech/sdk/http/reference
[server SQL allowlist]: https://github.com/tursodatabase/libsql/blob/main/libsql-server/src/query_analysis.rs
[libsql transaction timeout]: https://github.com/tursodatabase/libsql/blob/main/libsql-server/src/connection/mod.rs
[cloud limitations]: https://docs.turso.tech/cloud/limitations
