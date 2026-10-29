# Query cost evidence

Engine image: `ghcr.io/tursodatabase/libsql-server@sha256:6dd3eb276d9d3604e4a48ac4a999a2e267814732d57d7e94c04ba71482333a67`

These are real local libSQL rows_read/rows_written response counters.
They are not mock counters or a claim about a particular hosted engine.
Only synthetic data in a disposable container was used.

Reproduce with `python3 scripts/bench-query-cost.py`.

| Case | Rows read | Rows written |
| --- | ---: | ---: |
| before_text_200 | 801 | 1,000 |
| before_identical_retry_200 | 601 | 600 |
| before_events_200 | 801 | 800 |
| before_unchanged_200_of_2000_receipts | 2,400 | 800 |
| before_unchanged_read_receipt | 1 | 1 |
| before_expiry_zero_of_100400 | 100,401 | 0 |
| one_time_migration_100400 | 403,904 | 100,906 |
| after_text_200_cold_target | 402 | 600 |
| after_identical_retry_200 | 200 | 0 |
| after_events_200 | 400 | 200 |
| after_unchanged_200_of_2000_receipts | 201 | 0 |
| after_three_receipt_changes | 205 | 5 |
| after_unchanged_read_receipt | 1 | 0 |
| after_expiry_zero_of_100400 | 202 | 0 |
| after_event_history_50_of_1000 | 51 | 0 |
| after_text_history_50_behind_1000_events | 1,251 | 0 |
| after_last_text_id | 2 | 0 |
| after_backlog_after_last_id | 5 | 0 |
| after_backlog_10_new | 42 | 0 |
| after_search_1_of_200 | 4 | 0 |

Expiry fixture: 100,400 messages across 100 targets, none expired.

The insert cases commit 200 messages in one transaction. The optimized
path uses the production MessageWriter. Cold-target resolution is included;
subsequent writes reuse its bounded target cache. Network/target foreign keys
remain enforced. An identical retry must write zero rows.

Baselines use the frozen v0.99.21 schema and insert SQL. The optimized
cases use the clustered schema-33 tenant Message table: PRIMARY KEY
(target, time, id) WITHOUT ROWID, plus a text-only (target, id) index.
Tenants reserve every message ID through one high-water row, so expiry
no longer records the highest ID. The one-time migration case upgrades the
schema-31 fixture, including the clustered copy and sparse FTS rebuild; it
preserves all 100,400 messages and passes FTS integrity checks.

The read cases run after the expiry fixture adds 1,000 newer events to
each target. Text-only history steps over those events: the clustered
layout trades these cheaper reads for one fewer index write per message.
Setup, cleanup and validation queries are outside the measured intervals.


## Native SQLite plans and VM steps

SQLite 3.54.0; 100,000 messages, 100 targets, 99% events.
VM steps are execution work, not billable row counters.

| High-water statement | VM steps |
| --- | ---: |
| Before: aggregate with HAVING | 400,026 |
| After: descending seek, LIMIT 1 | 26 |

| Query family | EXPLAIN QUERY PLAN |
| --- | --- |
| Account name | `SEARCH User USING COVERING INDEX sqlite_autoindex_User_1 (username=?)` |
| Certificate fingerprint | `SEARCH DeviceCertificate USING COVERING INDEX sqlite_autoindex_DeviceCertificate_1 (fingerprint=?)` |
| Certificates by user | `SEARCH DeviceCertificate USING COVERING INDEX DeviceCertificate_user_index (user=?)` |
| Networks by owner | `SEARCH Network USING COVERING INDEX sqlite_autoindex_Network_2 (user=?)` |
| Channels by network | `SEARCH Channel USING COVERING INDEX sqlite_autoindex_Channel_1 (network=?)` |
| Receipt snapshot by client | `SEARCH DeliveryReceipt USING INDEX DeliveryReceipt_client_index (network=? AND client=?)` |
| Read receipt | `SEARCH ReadReceipt USING INDEX sqlite_autoindex_ReadReceipt_1 (network=? AND target=?)` |
| Web Push by owner/network | `SEARCH WebPushSubscription USING COVERING INDEX WebPushSubscription_user_network_index (user=? AND network=?)` |
| Web Push by endpoint | `SEARCH WebPushSubscription USING COVERING INDEX sqlite_autoindex_WebPushSubscription_1 (network=? AND endpoint=?)` |
| Target flags | `SEARCH MessageTarget USING INDEX MessageTarget_flags_index (network=?)` |
| Message ID cursor | `SEARCH Message USING COVERING INDEX Message_target_index (target=?)` |
| Text history | `SEARCH Message USING INDEX MessageTextIndex (target=?)` |
| Event playback merge | `MERGE (UNION ALL); LEFT; SEARCH Message USING INDEX MessageEventIndex (target=?); RIGHT; SEARCH Message USING INDEX MessageTextIndex (target=?)` |
| FTS candidate IDs | `SCAN MessageFTS VIRTUAL TABLE INDEX 0:M1` |
| Expiry candidates | `COMPOUND QUERY; LEFT-MOST SUBQUERY; SCAN t USING COVERING INDEX sqlite_autoindex_MessageTarget_1; SEARCH m USING INDEX MessageEventIndex (target=? AND time<?); UNION ALL; SCAN t USING COVERING INDEX sqlite_autoindex_MessageTarget_1; SEARCH m USING INDEX MessageTextIndex (target=? AND time<?)` |
| Foreign-key network lookup | `SEARCH MessageTarget USING COVERING INDEX sqlite_autoindex_MessageTarget_1 (network=? AND target=?)` |
| Statistics (intentional count) | `SCAN MessageTarget USING COVERING INDEX sqlite_autoindex_MessageTarget_1` |

The expiry plan scans target metadata and seeks bounded message-time ranges;
it does not scan all fresh messages. The high-water plan may say SCAN while
its descending LIMIT stops after one row, hence the separate VM measurement.
