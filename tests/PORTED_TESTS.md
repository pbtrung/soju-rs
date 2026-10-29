# Ported tests

Every `#[test]`/`#[tokio::test]` of soju-rs `main` (v0.99.23, `c350574`)
and its C counterpart in `tests/unit/` (`fn x` → `test_x`), or the phase
that will port it (PLAN.md section 5). C-only tests (ownership, error
messages, limits) are not listed. The 17 integration scenarios of
`tests/server.rs` (each run on both Tokio runtimes) are ported in phase 6
(L2), in both DB modes.

Ported so far: 86 of 165 unit tests (phases 0–3); 0 of 17 scenarios.

Also compared against the Rust code by the differential harness
`fuzz/diff_oracle.c` (oracle: `tests/oracle`, built by `tools/oracle.sh`).

| Rust file | Rust test | C test |
|---|---|---|
| `src/auth.rs` | `internal` | pending: phase 9 |
| `src/auth.rs` | `password_upgrade_kept` | pending: phase 9 |
| `src/auth.rs` | `escape` | pending: phase 9 |
| `src/auth.rs` | `http_auth` | pending: phase 9 |
| `src/bin/sojuctl.rs` | `turso_bootstrap_validates_before_connecting` | pending: phase 10 |
| `src/bin/sojuctl.rs` | `quoting` | pending: phase 10 |
| `src/bin/sojuctl.rs` | `quoting_roundtrip` | pending: phase 10 |
| `src/certfp.rs` | `generate_all` | `test_net_misc.c`: `test_generate_all` |
| `src/config.rs` | `turso_config` | `test_config_config.c`: `test_turso_config` |
| `src/config.rs` | `full_config` | `test_config_config.c`: `test_full_config` |
| `src/config.rs` | `defaults` | `test_config_config.c`: `test_defaults` |
| `src/config.rs` | `errors` | `test_config_config.c`: `test_errors` |
| `src/config.rs` | `durations` | `test_config_config.c`: `test_durations` |
| `src/config.rs` | `log_alias` | `test_config_config.c`: `test_log_alias` |
| `src/config.rs` | `fs_message_store_unsupported` | `test_config_config.c`: `test_fs_message_store_unsupported` |
| `src/conn.rs` | `stream_roundtrip` | `test_net_conn.c`: `test_stream_roundtrip` |
| `src/conn.rs` | `rate_limited` | `test_net_conn.c`: `test_rate_limited` |
| `src/conn.rs` | `queued_line` | `test_net_conn.c`: `test_queued_line` |
| `src/conn.rs` | `stream_wire_format` | `test_net_conn.c`: `test_stream_wire_format` |
| `src/conn.rs` | `websocket_wire_format` | pending: phase 8 (WebSocket) |
| `src/conn.rs` | `queue_budget` | `test_net_conn.c`: `test_queue_budget` |
| `src/conn.rs` | `input_in_flight` | `test_net_conn.c`: `test_input_in_flight` |
| `src/conn.rs` | `view` | `test_net_conn.c`: `test_view` |
| `src/conn.rs` | `line_parsing` | `test_net_conn.c`: `test_line_parsing` |
| `src/database/connection.rs` | `stream_redirect_cannot_expose_the_group_token_to_an_untrusted_origin` | pending: phase 2 |
| `src/database/mod.rs` | `network_url` | `test_net_url.c`: `test_network_url` |
| `src/database/mod.rs` | `host_port` | `test_net_url.c`: `test_host_port` |
| `src/database/mod.rs` | `password` | `test_db_sqlite.c`: `test_password` |
| `src/database/sqlite/tests.rs` | `users` | `test_db_sqlite.c`: `test_users` |
| `src/database/sqlite/tests.rs` | `networks_channels` | `test_db_sqlite.c`: `test_networks_channels` |
| `src/database/sqlite/tests.rs` | `messages` | `test_db_sqlite.c`: `test_messages` |
| `src/database/sqlite/tests.rs` | `messages_same_time` | `test_db_sqlite.c`: `test_messages_same_time` |
| `src/database/sqlite/tests.rs` | `message_ids_not_reused` | `test_db_sqlite.c`: `test_message_ids_not_reused` |
| `src/database/sqlite/tests.rs` | `delete_messages_in_chunks` | `test_db_sqlite.c`: `test_delete_messages_in_chunks` |
| `src/database/sqlite/tests.rs` | `message_last_id_insertion_order` | `test_db_sqlite.c`: `test_message_last_id_insertion_order` |
| `src/database/sqlite/tests.rs` | `delete_messages_vacuum` | `test_db_sqlite.c`: `test_delete_messages_vacuum` |
| `src/database/sqlite/tests.rs` | `messages_invalid_utf8` | `test_db_sqlite.c`: `test_messages_invalid_utf8` |
| `src/database/sqlite/tests.rs` | `receipts_targets_webpush_certs` | `test_db_sqlite.c`: `test_receipts_targets_webpush_certs` |
| `src/database/sqlite/tests.rs` | `fts_quote` | `test_db_sqlite.c`: `test_fts_quote` |
| `src/database/sqlite/tests.rs` | `migrations` | `test_db_sqlite.c`: `test_migrations` |
| `src/database/sqlite/tests.rs` | `message_text_index_migration` | `test_db_sqlite.c`: `test_message_text_index_migration` |
| `src/database/sqlite/tests.rs` | `message_text_index_query_plans` | `test_db_sqlite.c`: `test_message_text_index_query_plans` |
| `src/database/sqlite/tests.rs` | `newer_schema_rejected` | `test_db_sqlite.c`: `test_newer_schema_rejected` |
| `src/database/sqlite/tests.rs` | `unchanged_records_and_receipts_do_not_write` | `test_db_sqlite.c`: `test_unchanged_records_and_receipts_do_not_write` |
| `src/database/sqlite/tests.rs` | `sparse_fts_preserves_updates_and_event_history_ordering` | `test_db_sqlite.c`: `test_sparse_fts_preserves_updates_and_event_history_ordering` |
| `src/database/turso/http.rs` | `usage_counts_execute_and_batch_results_once` | pending: phase 9b |
| `src/database/turso/tests.rs` | `provisioning_authentication_isolation_and_restart` | pending: phase 9b |
| `src/database/turso/tests.rs` | `creation_and_deletion_resume_after_failure` | pending: phase 9b |
| `src/database/turso/tests.rs` | `wrong_group_and_sql_failure_leave_no_partial_message` | pending: phase 9b |
| `src/database/turso/tests.rs` | `names_retain_all_128_bits` | pending: phase 9b |
| `src/database/turso/tests.rs` | `authorization_failures_do_not_expose_credentials` | pending: phase 9b |
| `src/database/turso/tests.rs` | `user_schema_migration_preserves_messages` | pending: phase 9b |
| `src/database/turso/tests.rs` | `missing_ready_database_is_not_recreated_empty` | pending: phase 9b |
| `src/database/turso/tests.rs` | `invalid_links_and_changed_groups_cannot_delete_other_databases` | pending: phase 9b |
| `src/database/turso/tests.rs` | `tenant_owner_mismatch_stops_startup` | pending: phase 9b |
| `src/database/turso/tests.rs` | `lost_or_malformed_stream_never_continues_a_transaction` | pending: phase 9b |
| `src/database/turso/tests.rs` | `commit_closes_stream_on_hosted_and_legacy_servers` | pending: phase 9b |
| `src/database/turso/tests.rs` | `failed_begin_releases_stream_and_ambiguous_commit_is_not_replayed` | pending: phase 9b |
| `src/database/turso/tests.rs` | `malformed_commit_responses_fail_without_replaying_writes` | pending: phase 9b |
| `src/database/turso/tests.rs` | `binary_values_named_parameters_and_replication_index` | pending: phase 9b |
| `src/database/turso/tests.rs` | `slow_http_request_does_not_block_an_independent_stream` | pending: phase 9b |
| `src/database/turso/tests.rs` | `slow_storage_does_not_freeze_the_user_runtime_or_user_directory` | pending: phase 9b |
| `src/database/turso/tests.rs` | `cross_tenant_network_update_cannot_poison_routes_after_restart` | pending: phase 9b |
| `src/database/turso/tests.rs` | `startup_and_recurring_expiry_never_send_file_maintenance_sql` | pending: phase 9b |
| `src/database/turso/tests.rs` | `buffered_history_search_targets_and_shutdown_include_pending_lines` | pending: phase 9b |
| `src/database/turso/tests.rs` | `a_slow_commit_does_not_block_the_other_buffer_or_history_reads` | pending: phase 9b |
| `src/database/turso/tests.rs` | `atomic_message_batches_rollback_and_reject_conflicting_reserved_ids` | pending: phase 9b |
| `src/database/turso/tests.rs` | `retry_lost_batch_commit_response_without_duplicate_lines` | pending: phase 9b |
| `src/database/turso/tests.rs` | `encoded_json_body_budget_splits_long_escaped_lines` | pending: phase 9b |
| `src/database/turso/tests.rs` | `split_and_retry_host_body_and_processing_limits` | pending: phase 9b |
| `src/database/turso/tests.rs` | `reserved_message_ids_are_not_reused_after_restart_or_expiry` | pending: phase 9b |
| `src/database/turso/tests.rs` | `background_commits_keep_the_same_user_event_loop_responsive` | pending: phase 9b |
| `src/database/turso/tests.rs` | `production_message_path_commits_chathistory_limit_lines_per_http_batch` | pending: phase 9b |
| `src/database/turso/tests.rs` | `partial_message_batches_flush_on_the_timer` | pending: phase 9b |
| `src/database/turso/tests.rs` | `a_deleted_network_is_not_resurrected_by_buffered_writes` | pending: phase 9b |
| `src/database/turso/tests.rs` | `a_large_logical_multiline_message_fits_the_encoded_body_budget` | pending: phase 9b |
| `src/database/turso/tests.rs` | `pending_and_committed_history_merge_in_timestamp_and_id_order` | pending: phase 9b |
| `src/database/turso/tests.rs` | `failed_background_batches_retain_both_buffers_until_recovery` | pending: phase 9b |
| `src/database/turso/tests.rs` | `initialize_an_existing_empty_controller_with_read_only_user_version` | pending: phase 9b |
| `src/database/turso/tests.rs` | `existing_legacy_controller_imports_its_schema_version_and_keeps_accounts` | pending: phase 9b |
| `src/database/turso/tests.rs` | `tenant_user_removal_and_sparse_search_resume_preserve_existing_data` | pending: phase 9b |
| `src/database/utils.rs` | `strip` | `test_db_utils.c`: `test_strip` |
| `src/downstream/mod.rs` | `usernames` | pending: phase 4 |
| `src/downstream/mod.rs` | `nickserv` | pending: phase 4 |
| `src/downstream/mod.rs` | `network_attrs_roundtrip` | pending: phase 4 |
| `src/downstream/sasl.rs` | `sasl_mechanisms` | pending: phase 4 |
| `src/flags.rs` | `parse` | `test_config_flags.c`: `test_parse` |
| `src/flags.rs` | `errors` | `test_config_flags.c`: `test_errors` |
| `src/identd.rs` | `query` | `test_net_misc.c`: `test_query` |
| `src/irc/mod.rs` | `parse_basic` | `test_irc_message.c`: `test_parse_basic` |
| `src/irc/mod.rs` | `parse_tags` | `test_irc_message.c`: `test_parse_tags` |
| `src/irc/mod.rs` | `parse_many_tags` | `test_irc_message.c`: `test_parse_many_tags` |
| `src/irc/mod.rs` | `parse_errors` | `test_irc_message.c`: `test_parse_errors` |
| `src/irc/mod.rs` | `format_trailing` | `test_irc_message.c`: `test_format_trailing` |
| `src/irc/mod.rs` | `empty_prefix_not_formatted` | `test_irc_message.c`: `test_empty_prefix_not_formatted` |
| `src/irc/mod.rs` | `prefix_parse` | `test_irc_message.c`: `test_prefix_parse` |
| `src/ircutil.rs` | `user_modes_non_ascii` | `test_irc_ircutil.c`: `test_user_modes_non_ascii` |
| `src/ircutil.rs` | `highlight` | `test_irc_ircutil.c`: `test_highlight` |
| `src/ircutil.rs` | `split` | `test_irc_ircutil.c`: `test_split` |
| `src/ircutil.rs` | `user_modes` | `test_irc_ircutil.c`: `test_user_modes` |
| `src/ircutil.rs` | `channel_modes` | `test_irc_ircutil.c`: `test_channel_modes` |
| `src/ircutil.rs` | `member_prefix` | `test_irc_ircutil.c`: `test_member_prefix` |
| `src/listen.rs` | `uris` | `test_net_listen_uri.c`: `test_uris` |
| `src/listen.rs` | `forwarded` | `test_http_forwarded.c`: `test_forwarded` |
| `src/listen.rs` | `websocket_large_lines` | pending: phase 8 |
| `src/listen.rs` | `client_cert_header` | `test_http_forwarded.c`: `test_client_cert_header` |
| `src/metrics.rs` | `render` | pending: phase 8 |
| `src/msgstore/bare.rs` | `roundtrip` | `test_msgstore_msgid.c`: `test_roundtrip` |
| `src/msgstore/memory.rs` | `ring` | pending: phase 6 |
| `src/msgstore/memory.rs` | `delete_network` | pending: phase 6 |
| `src/msgstore/mod.rs` | `go_compat` | `test_msgstore_msgid.c`: `test_go_compat` |
| `src/proxyproto.rs` | `no_header` | `test_net_misc.c`: `test_no_header` |
| `src/proxyproto.rs` | `v1` | `test_net_misc.c`: `test_v1` |
| `src/proxyproto.rs` | `v2` | `test_net_misc.c`: `test_v2` |
| `src/rate.rs` | `backoff` | `test_net_rate.c`: `test_backoff` |
| `src/rate.rs` | `limiter` | `test_net_rate.c`: `test_limiter` |
| `src/scfg.rs` | `basic` | `test_config_scfg.c`: `test_basic` |
| `src/scfg.rs` | `quoting` | `test_config_scfg.c`: `test_quoting` |
| `src/scfg.rs` | `errors` | `test_config_scfg.c`: `test_errors` |
| `src/server.rs` | `create_user_on_auth_concurrently` | pending: phase 6 |
| `src/server.rs` | `msg_store_not_reloaded` | pending: phase 6 |
| `src/service/mod.rs` | `lookup` | pending: phase 7 |
| `src/service/mod.rs` | `help_listing` | pending: phase 7 |
| `src/service/mod.rs` | `durations` | pending: phase 7 |
| `src/tls.rs` | `pinned_cert` | `test_net_misc.c`: `test_pinned_cert` |
| `src/upstream/mod.rs` | `errno_messages` | pending: phase 5 |
| `src/upstream/mod.rs` | `labels` | pending: phase 5 |
| `src/upstream/mod.rs` | `registration_errors` | pending: phase 5 |
| `src/upstream/mod.rs` | `caps_requested` | pending: phase 5 |
| `src/upstream/mod.rs` | `cap_ls_continuation` | pending: phase 5 |
| `src/upstream/mod.rs` | `whox_fields` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `whois_no_such_nick` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `who_try_again` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `auto_detach` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `labeled_nested_batch` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `registration_timeout` | pending: phase 5 |
| `src/upstream/server_tests.rs` | `sasl_cap_nak_ends_capability_negotiation` | pending: phase 5 |
| `src/user/delivered.rs` | `store` | pending: phase 6 |
| `src/user/mod.rs` | `local_addr` | pending: phase 6 |
| `src/user/mod.rs` | `highlight` | pending: phase 6 |
| `src/user/mod.rs` | `ident` | pending: phase 6 |
| `src/util.rs` | `go_match_tests` | `test_base_path_match.c`: `test_go_match_tests` |
| `src/webpush.rs` | `encrypt_roundtrip` | pending: phase 9 |
| `src/webpush.rs` | `vapid` | pending: phase 9 |
| `src/webpush.rs` | `internal_ips` | pending: phase 9 |
| `src/webpush.rs` | `external_url` | pending: phase 9 |
| `src/webpush.rs` | `expired_status` | pending: phase 9 |
| `src/xirc/casemapping.rs` | `casemaps` | `test_irc_casemapping.c`: `test_casemaps` |
| `src/xirc/casemapping.rs` | `map_shrinks` | `test_irc_casemapping.c`: `test_map_shrinks` |
| `src/xirc/casemapping.rs` | `map` | `test_irc_casemapping.c`: `test_map` |
| `src/xirc/casemapping.rs` | `map_spellings` | `test_irc_casemapping.c`: `test_map_spellings` |
| `src/xirc/genmsg.rs` | `join_sorting_and_split` | `test_irc_genmsg.c`: `test_join_sorting_and_split` |
| `src/xirc/genmsg.rs` | `sasl_chunks` | `test_irc_genmsg.c`: `test_sasl_chunks` |
| `src/xirc/genmsg.rs` | `isupport` | `test_irc_genmsg.c`: `test_isupport` |
| `src/xirc/genmsg.rs` | `names` | `test_irc_genmsg.c`: `test_names` |
| `src/xirc/genmsg.rs` | `monitor` | `test_irc_genmsg.c`: `test_monitor` |
| `src/xirc/mod.rs` | `server_time` | `test_base_timeutil.c`: `test_server_time` |
| `src/xirc/mod.rs` | `membership_set` | `test_irc_xirc.c`: `test_membership_set` |
| `src/xirc/mod.rs` | `isupport_value` | `test_irc_xirc.c`: `test_isupport_value` |
| `src/xirc/multiline.rs` | `limits` | `test_irc_multiline.c`: `test_limits` |
| `src/xirc/multiline.rs` | `split` | `test_irc_multiline.c`: `test_split` |
| `src/xirc/multiline.rs` | `assemble` | `test_irc_multiline.c`: `test_assemble` |
| `src/xirc/multiline.rs` | `errors` | `test_irc_multiline.c`: `test_errors` |
| `src/xirc/whox.rs` | `options` | `test_irc_whox.c`: `test_options` |
| `src/xirc/whox.rs` | `roundtrip` | `test_irc_whox.c`: `test_roundtrip` |

## Integration scenarios (`tests/server.rs`, phase 6)

- `broadcast`: pending
- `chat_history_db`: pending
- `statusmsg_keeps_sigil`: pending
- `upstream_malformed_messages`: pending
- `cap_req_removal`: pending
- `labeled_response_ack`: pending
- `sasl_errors`: pending
- `chat_history_params`: pending
- `read_marker`: pending
- `upstream_clock_skew`: pending
- `multiline`: pending
- `service_other_user`: pending
- `chat_history_limit`: pending
- `service_blocking_work`: pending
- `monitor_invalid_targets`: pending
- `multiplex`: pending
- `multiplex_unsupported`: pending
