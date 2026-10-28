# soju-rs: why `MessageTextIndex` helps

```sql
CREATE INDEX IF NOT EXISTS MessageTextIndex ON Message(target, time) WHERE text IS NOT NULL;
ANALYZE;
```

Checked against soju-rs commit `dbb2c7c` (2026-10-24).

Implemented in soju-rs v0.99.18 (schema version 31). Fresh databases include
the index, and the migration adds it to existing databases with `IF NOT EXISTS`
and runs `ANALYZE MessageTextIndex`. An index previously added manually is
preserved. The automated tests verify migration from schema 30 and query plans
for text-only history and event playback, with and without planner statistics.

## 1. Symptom

- When a CHATHISTORY client (gamja, Goguma, senpai, …) connects, CPU jumps from 1–5% to 15–20% on a 0.2 vCPU VPS, and memory spikes.
- The memory is all `file` (page cache) in `memory.stat`. `anon` and RSS stay flat.
- WeeChat connects without any spike.

## 2. Evidence from the source

**E1: Events are stored with `text = NULL`.**
`src/database/sqlite/messages.rs` (store path, about line 144):

```rust
let text = match msg.command.as_str() {
    "PRIVMSG" | "NOTICE" if msg.params.len() > 1 => Some(strip_ansi(&msg.params[1])),
    _ => None,
};
```

So JOIN, PART, QUIT, NICK, MODE, TOPIC and so on all get `text IS NULL`. In busy channels, these often make up most of the rows.

**E2: The history queries filter on `text IS NOT NULL`.**

- `list_message_last_per_target` (line 165, filter at line 176). This serves `CHATHISTORY TARGETS`:
  ```sql
  SELECT target, (SELECT time FROM Message
                  WHERE target = MessageTarget.id AND text IS NOT NULL
                  ORDER BY time DESC LIMIT 1) latest
  FROM MessageTarget WHERE network = :network ... ORDER BY latest ... LIMIT :limit
  ```
  The subquery runs for every `MessageTarget` of the network, including old DMs, before `LIMIT` applies.
- `list_messages` (line 232, filter at line 279). This serves `CHATHISTORY LATEST/BEFORE/AFTER/...`:
  ```sql
  ... WHERE m.target = t.id AND t.network = :network AND t.target = :target
      AND m.text IS NOT NULL
  ORDER BY m.time DESC, m.id DESC LIMIT :limit
  ```

**E3: The only usable index doesn't contain `text`.**
`src/database/sqlite_schema.sql`:

```sql
CREATE INDEX MessageIndex ON Message(target, time);
CREATE INDEX Message_target_index ON Message(target);
```

**E4: SQLite's own cache is tiny.**
`src/database/sqlite/mod.rs:84` sets `PRAGMA cache_size = -512`, which is 512 KB. Almost every page read goes to the OS, which means `pread()` and then the kernel page cache. This is why the spike shows up as `file` memory.

**E5: The filter only applies when the client does *not* enable `draft/event-playback`.**
`src/downstream/cmds/history.rs:146–152`:

```rust
let event_playback = dc!(u, i).caps.is_enabled("draft/event-playback");
... events: event_playback,
```

`text IS NOT NULL` is added only when `events == false`. (See caveat C1.)

**E6: WeeChat never sends CHATHISTORY**, so none of these queries run for it. That explains why there's no spike with WeeChat.

## 3. Reasoning

### Without the partial index

To answer "newest message with text in target X", SQLite uses `MessageIndex(target, time)` and walks it backwards from the newest entry. The index has no `text` column, so for **every entry** SQLite must open the table row (B-tree lookup by rowid) and check `text IS NOT NULL`:

```
index → row: QUIT  (text NULL) skip
index → row: JOIN  (text NULL) skip
... N events ...
index → row: PRIVMSG  match
```

Cost per lookup is about (number of events between real messages + wanted rows) table-row reads.

Rows are stored in arrival (`id`) order with all channels mixed together. Rows
for one target can therefore be scattered across pages (commonly 4 KB). The
number of additional page reads depends on row size, interleaving and cache
state; skipping an event does not necessarily require a separate disk read.
The extra work includes:

- **CPU**: B-tree descent, plus page copy, plus row decode, for every skipped event.
- **Page cache**: every distinct page touched is loaded into the kernel cache. It stays there until it's reclaimed, which is the "releases after a while" behavior.

On connect, a CHATHISTORY client typically does `TARGETS` (one subquery per target, see E2), then `LATEST <limit>` per buffer. The waste is multiplied by the number of targets.

### With the partial index

`... WHERE text IS NOT NULL` builds an index containing **only rows with text**. Event rows are never put into it.

- SQLite can use a partial index when the query's WHERE clause implies the index's WHERE clause. Both queries contain `text IS NOT NULL` literally (E2), so the condition holds.
- `TARGETS` subquery: one seek to the last entry for `target` in `MessageTextIndex` returns `time` directly. The column is in the index, so no table row is read at all.
- `LATEST n`: walking `MessageTextIndex` backwards returns a matching row on every step. That's exactly `n` row reads, with zero event rows skipped.

The basic text-only history lookup no longer walks intervening event rows.
Seeks, sorting targets and any additional filters still contribute to query
cost; extra filters can require examining more text messages than the limit.

### What `ANALYZE` does

It writes statistics to `sqlite_stat1` to help the query planner choose between
indexes. The planner also picks the partial index without statistics in the
automated test fixture. A full `ANALYZE` can scan unrelated tables and indexes;
the migration uses `ANALYZE MessageTextIndex` to restrict that work to the new
index. Index creation and analysis are one-time startup work on existing data.
See SQLite's [partial-index rules](https://www.sqlite.org/partialindex.html)
and [ANALYZE documentation](https://www.sqlite.org/lang_analyze.html).

## 4. Evidence from tests

### Query plans (soju-rs schema, `EXPLAIN QUERY PLAN`)

Before:

```
SEARCH MessageTarget USING COVERING INDEX sqlite_autoindex_MessageTarget_1 (network=?)
CORRELATED SCALAR SUBQUERY 1
SEARCH Message USING INDEX MessageIndex (target=?)
```

After:

```
SEARCH MessageTarget USING COVERING INDEX sqlite_autoindex_MessageTarget_1 (network=?)
CORRELATED SCALAR SUBQUERY 1
SEARCH Message USING INDEX MessageTextIndex (target=?)
```

`list_messages` moves from `MessageIndex` to `MessageTextIndex` in the same way.

### Benchmark: simulated client connect

Setup: 400k messages, 60 targets, `cache_size=-512`, warm OS cache, fast CPU. One connect = `TARGETS`, then `LATEST 100` for each target.

| events among stored rows | before | after | speedup |
|---|---|---|---|
| 50% | 11.7 ms | 6.9 ms | 1.7× |
| 90% | 45.5 ms | 7.2 ms | 6.3× |
| 99% | 283.6 ms | 5.9 ms | 48× |

"After" stays flat regardless of event ratio, as the reasoning predicts. A 0.2 vCPU VPS with a cold disk will be slower in absolute terms, with a larger absolute saving.

## 5. Verify on your own server

Adjust the database path as needed.

**Step 1: Is your data event-heavy?**

```sh
sqlite3 /var/lib/soju/main.db \
"SELECT t.target, count(*) total, round(100.0*sum(m.text IS NULL)/count(*)) pct_events
 FROM Message m JOIN MessageTarget t ON t.id = m.target
 GROUP BY t.id ORDER BY total DESC LIMIT 15;"
```

High `pct_events` (≥80%) means a large expected gain. Low means a small one.

**Step 2: Does your client enable `draft/event-playback`?**
If it does, the filter isn't applied and the index won't be used (C1). Check the client's CAP REQ in its raw log, or soju's debug log.

**Step 3: Baseline timing (before the index).**
Pick a busy channel. Find its network id with `SELECT id, network, target FROM MessageTarget;`.

```sh
sqlite3 /var/lib/soju/main.db <<'EOF'
.timer on
.eqp on
SELECT target, (SELECT time FROM Message WHERE target = MessageTarget.id AND text IS NOT NULL
                ORDER BY time DESC LIMIT 1) latest
FROM MessageTarget WHERE network = 1 ORDER BY latest DESC LIMIT 500;
SELECT count(*) FROM (SELECT m.raw FROM Message m, MessageTarget t
  WHERE m.target = t.id AND t.network = 1 AND t.target = '#busychannel' AND m.text IS NOT NULL
  ORDER BY m.time DESC, m.id DESC LIMIT 100);
EOF
```

`.eqp on` should show `USING INDEX MessageIndex`.

Optional cold-cache I/O measurement: run `vmtouch -e main.db*` (or `echo 1 > /proc/sys/vm/drop_caches` on a test box), then compare `vmtouch main.db` resident pages before and after running the queries.

**Step 4: Create the index** (stop soju, back up first).

```sh
systemctl stop soju
cp /var/lib/soju/main.db /var/lib/soju/main.db.bak
sqlite3 /var/lib/soju/main.db \
  "CREATE INDEX IF NOT EXISTS MessageTextIndex ON Message(target, time) WHERE text IS NOT NULL; ANALYZE;"
```

**Step 5: Repeat step 3.**

- `.eqp` should now show `USING INDEX MessageTextIndex`.
- Run time should drop roughly in line with your event ratio from step 1.
- Resident pages touched (vmtouch) should drop the same way.

**Step 6: Real-world check.**
Start soju and connect your CHATHISTORY client. Compare the CPU peak (`top`/`pidstat -p $(pidof soju) 1`) and `file` in `memory.stat` against the earlier numbers.

**Rollback:**

```sql
DROP INDEX MessageTextIndex;
```

## 6. Caveats

- **C1: Clients using `draft/event-playback`** query without the `text` filter, so they keep using `MessageIndex`. There's no gain for them, and also no regression.
- **C2: Row reads for the requested messages remain.** `LATEST 100` still reads 100 scattered table rows, since `raw` isn't in the index. A lower client fetch limit and message retention reduce that part.
- **C3: `TARGETS` still runs one lookup per `MessageTarget`.** Each lookup is now cheap (index-only), but many stale DM targets still add up. Retention, or deleting old targets, helps.
- **C4: Costs.** The index adds a little disk space (about 30–50 bytes per text message) and makes inserts slightly slower. Both are negligible for IRC volumes.
- **C5: Schema safety.** Since v0.99.18, the index is managed by soju-rs's schema and migrations. The schema version rises from 30 to 31, so older binaries supporting only schema 30 reject the upgraded database. `IF NOT EXISTS` handles an index already created manually. A future migration rebuilding `Message` must recreate this index too. Dropping it manually does not lower the schema version or cause it to be recreated on every startup.
- **C6: Page cache itself is harmless.** It's reclaimable memory. The fix reduces it because it reduces the I/O that creates it.
