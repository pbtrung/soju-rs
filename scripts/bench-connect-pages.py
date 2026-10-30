#!/usr/bin/env python3
"""Count the database pages a CHATHISTORY client's connect reads.

Builds two files with the same synthetic history: the local layout of
schema 33 (rowid Message table, frozen below) and the current one (read
from src/database/sqlite_schema.sql). Then replays what gamja sends on
connect: CHATHISTORY TARGETS, then the latest text messages of each
target. The sqlite3 shell reports page cache misses with an unbounded
cache, i.e. the distinct pages read from the file: what fills the kernel
page cache on a server.

Requires the sqlite3 shell. Uses only synthetic data, in a temporary
directory.
"""

import argparse
import random
import sqlite3
import subprocess
import tempfile
from pathlib import Path

SCHEMA_33 = """
CREATE TABLE MessageTarget (id INTEGER PRIMARY KEY, network INTEGER NOT NULL,
    target TEXT NOT NULL, UNIQUE(network, target));
CREATE TABLE Message (id INTEGER PRIMARY KEY, target INTEGER NOT NULL,
    raw TEXT NOT NULL, time TEXT NOT NULL, sender TEXT NOT NULL, text TEXT);
CREATE INDEX MessageEventIndex ON Message(target, time) WHERE text IS NULL;
CREATE INDEX MessageTextIndex ON Message(target, time) WHERE text IS NOT NULL;
CREATE INDEX Message_target_index ON Message(target);
"""


def current_schema(root):
    # Only the history tables: the others don't change between layouts
    sql = (root / "src/database/sqlite_schema.sql").read_text()
    sql = sql[sql.index("CREATE TABLE Message (") :]
    sql = sql.replace(",\n\tFOREIGN KEY(target) REFERENCES MessageTarget(id)", "")
    sql = sql[: sql.index("CREATE TABLE MessageTarget")]
    return SCHEMA_33.split(";")[0] + ";\n" + sql


def history(messages, targets, events, seed):
    rng = random.Random(seed)
    # Busy channels get most of the traffic
    weights = [1 / (i + 1) for i in range(targets)]
    for i in range(1, messages + 1):
        target = rng.choices(range(1, targets + 1), weights)[0]
        event = rng.random() < events
        prefix = f"@time=2024-01-01T00:00:00.000Z :nick{i % 500}!user@host.example.org "
        if event:
            raw = prefix + "QUIT :Ping timeout: 240 seconds"
        else:
            raw = prefix + f"PRIVMSG #c{target} :" + "x" * rng.randint(20, 160)
        yield (i, target, raw, f"{i:012d}", "nick", None if event else "text")


def connect_queries(targets, limit, indexed_by):
    hint = "INDEXED BY MessageTextIndex" if indexed_by else ""
    yield (
        f"SELECT count(*) FROM (SELECT target, (SELECT time FROM Message {hint} "
        "WHERE target=MessageTarget.id AND text IS NOT NULL ORDER BY time DESC LIMIT 1) "
        "latest FROM MessageTarget WHERE network=1 ORDER BY latest DESC LIMIT 1000);"
    )
    for t in range(1, targets + 1):
        # sum(length(raw)) reads the rows, like the bouncer sending them
        yield (
            f"SELECT sum(length(raw)) FROM (SELECT m.raw FROM Message AS m {hint} "
            f"WHERE m.target=(SELECT id FROM MessageTarget WHERE network=1 AND target='#c{t}') "
            f"AND m.text IS NOT NULL ORDER BY time DESC, id DESC LIMIT {limit});"
        )


def pages_read(path, queries):
    script = "PRAGMA cache_size=-4000000;\n.stats on\n" + "\n".join(queries)
    out = subprocess.run(
        ["sqlite3", str(path)], input=script, capture_output=True, text=True, check=True
    ).stdout
    return sum(
        int(line.split(":")[1].split()[0])
        for line in out.splitlines()
        if line.startswith("Page cache misses")
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--messages", type=int, default=400_000)
    parser.add_argument("--targets", type=int, default=60)
    parser.add_argument("--limit", type=int, default=100)
    parser.add_argument("--events", type=float, nargs="+", default=[0.5, 0.9, 0.99])
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    layouts = [("schema 33", SCHEMA_33, False), ("schema 34", current_schema(root), True)]

    print(f"{args.messages:,} messages, {args.targets} targets, LATEST {args.limit} each\n")
    print("| Events | " + " | ".join(f"{name} pages | file KiB" for name, _, _ in layouts) + " |")
    print("| ---: |" + " ---: | ---: |" * len(layouts))
    with tempfile.TemporaryDirectory() as tmp:
        for events in args.events:
            rows = list(history(args.messages, args.targets, events, seed=1))
            cells = []
            for i, (_, schema, indexed_by) in enumerate(layouts):
                path = Path(tmp) / f"{events}-{i}.db"
                db = sqlite3.connect(path)
                db.executescript(schema)
                db.executemany(
                    "INSERT INTO MessageTarget VALUES(?, 1, ?)",
                    [(t, f"#c{t}") for t in range(1, args.targets + 1)],
                )
                db.executemany(
                    "INSERT INTO Message(id, target, raw, time, sender, text) "
                    "VALUES(?, ?, ?, ?, ?, ?)",
                    rows,
                )
                db.commit()
                db.execute("VACUUM")
                db.close()
                pages = pages_read(path, connect_queries(args.targets, args.limit, indexed_by))
                cells.append(f"{pages:,} | {path.stat().st_size // 1024:,}")
            print(f"| {events:.0%} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
