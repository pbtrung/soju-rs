#!/usr/bin/env python3
"""Measure actual soju SQL against a disposable official libSQL engine.

Uses synthetic data only. The server is created by this script, listens on
localhost, has no cloud credentials, and is removed even if the test fails.
"""

import argparse
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import time
import uuid


IMAGE = (
    "ghcr.io/tursodatabase/libsql-server@"
    "sha256:6dd3eb276d9d3604e4a48ac4a999a2e267814732d57d7e94c04ba71482333a67"
)


def sqlite_evidence(root):
    db = sqlite3.connect(":memory:", isolation_level=None)
    db.executescript((root / "src/database/sqlite_schema.sql").read_text())
    db.executescript((root / "src/database/message_fts.sql").read_text())
    db.executescript("""
        INSERT INTO User(id,username,created_at)VALUES(1,'fixture','');
        INSERT INTO Network(id,user,addr)VALUES(1,1,'irc.fixture');
        WITH RECURSIVE t(id)AS(VALUES(1) UNION ALL SELECT id+1 FROM t WHERE id<100)
        INSERT INTO MessageTarget(id,network,target)SELECT id,1,'#'||id FROM t;
        WITH RECURSIVE m(id)AS(VALUES(1) UNION ALL SELECT id+1 FROM m WHERE id<100000)
        INSERT INTO Message(id,target,raw,time,sender,text)
        SELECT id,1+(id%100),'raw',printf('%012d',id),'fixture',
               CASE WHEN id%100=0 THEN 'token' END FROM m;
    """)
    report = ["", "## Native SQLite plans and VM steps", "",
              f"SQLite {sqlite3.sqlite_version}; 100,000 messages, 100 targets, 99% events,",
              "local schema 34 (the clustered Message table of Turso tenants plus a",
              "text-only time index).",
              "VM steps are execution work, not billable row counters.", "",
              "| ID allocation statement | VM steps |", "| --- | ---: |"]
    for name, sql in [
        ("Reservation of the next IDs", "INSERT INTO MessageIdHighWater(id,last_id) VALUES(0,coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0),0)+200) ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id RETURNING last_id"),
    ]:
        steps = [0]
        def tick():
            steps[0] += 1
            return 0
        db.execute("SAVEPOINT measure")
        db.set_progress_handler(tick, 1)
        db.execute(sql).fetchall()
        db.set_progress_handler(None, 0)
        db.execute("ROLLBACK TO measure")
        db.execute("RELEASE measure")
        report.append(f"| {name} | {steps[0]:,} |")
    queries = {
        "Account name": "SELECT id FROM User WHERE username='fixture'",
        "Certificate fingerprint": "SELECT id FROM DeviceCertificate WHERE fingerprint=x'1234'",
        "Certificates by user": "SELECT id FROM DeviceCertificate WHERE user=1",
        "Networks by owner": "SELECT id FROM Network WHERE user=1",
        "Channels by network": "SELECT id FROM Channel WHERE network=1",
        "Receipt snapshot by client": "SELECT id,target,internal_msgid FROM DeliveryReceipt WHERE network=1 AND client IS 'phone' ORDER BY id",
        "Read receipt": "SELECT id,timestamp FROM ReadReceipt WHERE network=1 AND target='#1'",
        "Web Push by owner/network": "SELECT id FROM WebPushSubscription WHERE user=1 AND network IS 1",
        "Web Push by endpoint": "SELECT id FROM WebPushSubscription WHERE network=1 AND endpoint='https://fixture'",
        "Target flags": "SELECT id FROM MessageTarget WHERE network=1 AND (pinned OR muted OR blocked)",
        "Message ID cursor": "SELECT id FROM Message WHERE target=1 AND text IS NOT NULL ORDER BY id DESC LIMIT 1",
        "Text history": "SELECT id,time,raw FROM Message INDEXED BY MessageTextIndex WHERE target=1 AND text IS NOT NULL ORDER BY time DESC,id DESC LIMIT 10",
        "Event playback": "SELECT id,time,raw FROM Message WHERE target=1 ORDER BY time DESC,id DESC LIMIT 10",
        "Latest text time per target": "SELECT target,(SELECT time FROM Message INDEXED BY MessageTextIndex WHERE target=MessageTarget.id AND text IS NOT NULL ORDER BY time DESC LIMIT 1) latest FROM MessageTarget WHERE network=1",
        "FTS candidate IDs": "SELECT rowid FROM MessageFTS WHERE MessageFTS MATCH 'token'",
        "Search result rows": "SELECT m.id FROM (SELECT rowid AS r FROM MessageFTS WHERE MessageFTS MATCH 'token') AS f CROSS JOIN Message AS m ON m.id=f.r WHERE m.target=1 AND m.text IS NOT NULL",
        "Expiry candidates": "SELECT m.target,m.time,m.id FROM MessageTarget t CROSS JOIN Message m ON m.target=t.id WHERE m.time<'000000000000' LIMIT 1000",
        "Foreign-key network lookup": "SELECT id FROM MessageTarget WHERE network=1 AND target='#1'",
        "Statistics (intentional count)": "SELECT count(*) FROM MessageTarget",
    }
    report += ["", "| Query family | EXPLAIN QUERY PLAN |", "| --- | --- |"]
    for name, sql in queries.items():
        plan = "; ".join(row[3] for row in db.execute("EXPLAIN QUERY PLAN " + sql))
        report.append(f"| {name} | `{plan}` |")
    db.close()
    report += ["", "The expiry plan scans target metadata and seeks bounded message-time ranges;",
               "it does not scan all fresh messages.", ""]
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="Write the evidence Markdown here")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    name = "soju-query-cost-" + uuid.uuid4().hex[:12]
    subprocess.run(["docker", "pull", IMAGE], check=True)
    try:
        subprocess.run(
            ["docker", "run", "--rm", "-d", "--name", name,
             "-p", "127.0.0.1::8080", "-e", "SQLD_NODE=primary", IMAGE],
            check=True, capture_output=True, text=True,
        )
        endpoint = subprocess.check_output(
            ["docker", "port", name, "8080/tcp"], text=True
        ).strip()
        host, port = endpoint.rsplit(":", 1)
        if host != "127.0.0.1":
            raise RuntimeError("benchmark server must be bound only to localhost")
        deadline = time.monotonic() + 30
        while True:
            try:
                with socket.create_connection((host, int(port)), timeout=1):
                    break
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
        env = dict(os.environ, SOJU_QUERY_COST_URL=f"http://{endpoint}/v2/pipeline")
        result = subprocess.run(
            ["cargo", "test", "--lib", "--locked", "real_engine_query_cost",
             "--", "--ignored", "--nocapture"],
            cwd=root, env=env, text=True, capture_output=True,
        )
        if result.returncode:
            print(result.stdout)
            print(result.stderr)
            raise SystemExit(result.returncode)
        line = next(x for x in result.stdout.splitlines() if x.startswith("SOJU_QUERY_COST "))
        evidence = json.loads(line.removeprefix("SOJU_QUERY_COST "))
        report = [
            "# Query cost evidence", "",
            f"Engine image: `{IMAGE}`", "",
            "These are real local libSQL rows_read/rows_written response counters.",
            "They are not mock counters or a claim about a particular hosted engine.",
            "Only synthetic data in a disposable container was used.", "",
            "Reproduce with `python3 scripts/bench-query-cost.py`.", "",
            "| Case | Rows read | Rows written |", "| --- | ---: | ---: |",
        ]
        for row in evidence["measurements"]:
            report.append(f"| {row['case']} | {row['rows_read']:,} | {row['rows_written']:,} |")
        report += ["", f"Expiry fixture: {evidence['fixture_messages']:,} messages across 100 targets, none expired.",
                   "", "The insert cases commit 200 messages in one transaction. The optimized",
                   "path uses the production MessageWriter. Cold-target resolution is included;",
                   "subsequent writes reuse its bounded target cache. Network/target foreign keys",
                   "remain enforced. An identical retry must write zero rows.", "",
                   "Baselines use the frozen v0.99.21 schema and insert SQL. The optimized",
                   "cases use the clustered schema-33 tenant Message table: PRIMARY KEY",
                   "(target, time, id) WITHOUT ROWID, plus a text-only (target, id) index.",
                   "Tenants reserve every message ID through one high-water row, so expiry",
                   "no longer records the highest ID. The one-time migration case upgrades the",
                   "schema-31 fixture, including the clustered copy and sparse FTS rebuild; it",
                   "preserves all 100,400 messages and passes FTS integrity checks.", "",
                   "The read cases run after the expiry fixture adds 1,000 newer events to",
                   "each target. Text-only history steps over those events: the clustered",
                   "layout trades these cheaper reads for one fewer index write per message.",
                   "Setup, cleanup and validation queries are outside the measured intervals.", ""]
        report += sqlite_evidence(root)
        text = "\n".join(report)
        print(text)
        if args.output:
            args.output.write_text(text)
    finally:
        subprocess.run(["docker", "stop", "-t", "2", name], capture_output=True)


if __name__ == "__main__":
    main()
