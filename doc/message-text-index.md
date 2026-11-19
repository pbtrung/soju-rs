# Message storage and indexes

This guide describes schema 35. The [historical investigation](archive/message-text-index.md)
contains the measurements behind earlier index and clustering changes.

The implementation lives in the [schema](../src/database/sqlite_schema.sql),
[migrations](../src/database/sqlite_migrations.rs),
[message queries](../src/database/sqlite/messages.rs) and
[FTS triggers](../src/database/message_fts.sql).

## Current layout

SQLite stores `Message` as a `WITHOUT ROWID` table with primary key
`(target, time, id)`. Messages for a target are clustered in history order.
Rows contain the raw IRC message, its timestamp, sender and optional searchable
text. `PRIVMSG` and `NOTICE` have text; events such as JOIN and QUIT do not.

| Structure | Purpose |
| --- | --- |
| `Message` primary key | History including events, ordered by target, time and ID |
| `MessageTextIndex` on `(target, time) WHERE text IS NOT NULL` | Text-only history and latest text timestamps |
| `MessageTextIdIndex` on `(target, id) WHERE text IS NOT NULL` | Delivery cursors and text search result lookups |
| `MessageIdHighWater` | Transactional allocation of IDs that are not reused after deletion |
| `MessageFTS` | Full-text search over message text, maintained by triggers |
| `TargetHighlight` | Latest mention timestamp per network and target; see [unread state](unread.md) |

The old `MessageIndex`, `MessageEventIndex` and `Message_target_index` are not
part of the current message layout. Do not add or remove indexes using the
archived guide's pre-migration instructions.

## History queries

Text-only history uses `MessageTextIndex` to skip event rows. Queries explicitly
select this index where SQLite might otherwise prefer scanning the primary
key without planner statistics. Event playback uses the clustered table.
History replies sort equal timestamps by message ID.

`CHATHISTORY TARGETS` finds each target's latest timestamp and compares text,
mention and read-marker times. Text-only timestamp lookups can use the partial
index without loading raw messages. Event-inclusive listings also look up the
latest event in the clustered table. Sorting and filtering targets still have
a cost; a response limit does not eliminate every per-target lookup.

Delivery receipts use insertion IDs rather than assuming server timestamps
always increase. Full-text queries use FTS matches to locate text messages;
they remain scoped to the requested network and target.

## Upgrades and compatibility

Startup migrates supported older databases transactionally to schema 35.
Schema 34 rebuilt the message table in clustered order while preserving IDs;
schema 35 added mention tracking. Fresh databases use the current layout.

Back up an existing database before first opening it with a newer version.
Use a SQLite-consistent backup, or stop the bouncer before copying the database
and its outstanding WAL state. A table rebuild needs temporary disk space and
startup time proportional to the stored history. Earlier soju-rs versions and
upstream soju cannot open the upgraded schema 35 database; returning to an
older binary requires a compatible backup.

Do not lower `PRAGMA user_version` or drop required indexes as a rollback.
The [migration tests](../src/database/sqlite/tests/migrations.rs) cover legacy
data, index preservation, FTS consistency and reopening an upgraded database.

## Expiry and maintenance

With `message-expiry` and the database message store, expired messages are
deleted in chunks. The bouncer optimizes the search index after deletions.
On startup, during daily expiry, and when expiry changes on reload, it
vacuums if at least 25% of database pages are free. Below that threshold,
freed pages remain available for reuse. See
[maintenance code](../src/server/maintenance.rs).

VACUUM rewrites the database and needs extra disk space. A daily or reload-time
vacuum blocks other database access while it runs. Expiry frees database pages;
it does not necessarily shrink the file immediately.

For a manual VACUUM, stop the bouncer, make a consistent backup and ensure
sufficient free space first. Required indexes are managed by migrations.

## Measuring storage changes

From the repository root:

```sh
python3 scripts/bench-connect-pages.py --messages 400000 --targets 60 --limit 100
```

The script needs the `sqlite3` shell and uses synthetic temporary databases.
It compares a frozen schema 33 message layout with the current clustered
layout; its output labels that layout "schema 34", where clustering was
introduced. It measures SQLite page-cache misses and file sizes for target
listing followed by recent text history. It does not measure live-server
latency, RSS or physical disk reads under every OS cache state.

The [archived results](archive/message-text-index.md#pages-read-on-connect)
show that clustering's benefit depends on event density. Re-run the workload
for a proposed change rather than treating those figures as current results.

[Documentation index](README.md) · [Performance measurement](performance-layout.md)
