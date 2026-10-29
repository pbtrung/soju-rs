// Ported from soju-rs src/database/turso/tests.rs: Turso storage against the
// local Platform/Hrana mock (accounts, provisioning, recovery, isolation,
// streams and transactions)
#include "testsupport.h"

#include <yyjson.h>

#include "base/alloc.h"
#include "db/internal.h"
#include "db/turso.h"
#include "turso_mock.h"

static size_t databases(struct turso_mock *m) {
	pthread_mutex_lock(&m->mu);
	size_t n = strmap_len(&m->st.databases);
	pthread_mutex_unlock(&m->mu);
	return n;
}

static bool has_database(struct turso_mock *m, const char *name) {
	pthread_mutex_lock(&m->mu);
	bool has = strmap_has(&m->st.databases, name);
	pthread_mutex_unlock(&m->mu);
	return has;
}

static struct db_network network(struct db *db, int64_t user_id, const char *addr) {
	struct db_network n;
	db_network_init(&n, addr);
	struct err *e = nullptr;
	if (!db_store_network(db, user_id, &n, &e)) {
		fail_msg("store_network: %s", err_msg(e));
	}
	return n;
}

static void store(struct db *db, int64_t net, const char *target, const char *text, i64vec *ids) {
	struct irc_msg *m = IRC_MSG("PRIVMSG", target, text);
	i64vec ignored = {0};
	struct err *e = nullptr;
	if (!db_store_messages(db, net, target, &m, 1, ids != nullptr ? ids : &ignored, &e)) {
		fail_msg("store_messages: %s", err_msg(e));
	}
	vec_free(&ignored);
	irc_msg_free(m);
}

static irc_msg_vec list(struct db *db, int64_t net, const char *target, const char *text) {
	struct db_message_options o = {.limit = 10, .text = text};
	irc_msg_vec v = {0};
	assert_true(db_list_messages(db, net, target, &o, &v, nullptr));
	return v;
}

static void test_provisioning_authentication_isolation_and_restart(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice"), bob = turso_mock_user(db, "bob");
	struct db_user auth;
	bool found = false, upgraded = false;
	assert_true(db_get_user(db, "alice", &auth, &found, nullptr));
	assert_true(found);
	assert_true(auth.admin);
	assert_true(db_user_check_password(&auth, "secret", &upgraded, nullptr));
	struct err *e = nullptr;
	assert_false(db_user_check_password(&auth, "wrong", &upgraded, &e));
	err_free(e);
	db_user_free(&auth);
	struct db_network a_net = network(db, alice.id, "ircs://irc.example.org");
	struct db_network b_net = network(db, bob.id, "ircs://irc.example.org");
	assert_int_not_equal(a_net.id, b_net.id);
	i64vec ids = {0};
	store(db, a_net.id, "#a", "alice's searchable message", &ids);
	store(db, b_net.id, "#a", "bob private", nullptr);
	struct db_delivery_receipt r;
	db_delivery_receipt_init(&r);
	xfree(r.target);
	xfree(r.internal_msg_id);
	r.target = xstrdup("#a");
	r.internal_msg_id = xstrdup("cursor");
	assert_true(db_store_client_delivery_receipts(db, a_net.id, "phone", &r, 1, nullptr));
	db_delivery_receipt_free(&r);
	struct db_read_receipt read;
	db_read_receipt_init(&read);
	xfree(read.target);
	read.target = xstrdup("#a");
	read.timestamp = (struct db_time){.set = true, .t = timestamp_now()};
	assert_true(db_store_read_receipt(db, a_net.id, &read, nullptr));
	db_read_receipt_free(&read);
	struct db_channel channel;
	db_channel_init(&channel);
	xfree(channel.name);
	channel.name = xstrdup("#a");
	assert_true(db_store_channel(db, a_net.id, &channel, nullptr));
	struct db_device_certificate cert;
	db_device_certificate_init(&cert);
	xfree(cert.label);
	cert.label = xstrdup("phone");
	strbuf_append(&cert.fingerprint, "\x01\x02\x03", 3);
	cert.last_used = (struct db_time){.set = true, .t = timestamp_now()};
	assert_true(db_store_device_certificate(db, alice.id, &cert, nullptr));
	int64_t owner = 0;
	struct db_device_certificate got;
	assert_true(db_get_device_certificate(db, strbuf_str(&cert.fingerprint), &owner, &got,
					      &found, nullptr));
	assert_true(found);
	assert_int_equal(owner, alice.id);
	db_device_certificate_free(&got);
	cert.id = 0;
	e = nullptr;
	assert_false(db_store_device_certificate(db, bob.id, &cert, &e));
	assert_int_equal(e->code, DB_ERR_DUPLICATE_DEVICE_CERTIFICATE);
	err_free(e);
	db_device_certificate_free(&cert);
	// Database names: 25 characters a-z and 0-9, distinct
	AUTOFREE char *a_name = turso_mock_linked_name(db, alice.id);
	AUTOFREE char *b_name = turso_mock_linked_name(db, bob.id);
	assert_int_equal(strlen(a_name), 25);
	assert_string_not_equal(a_name, b_name);
	// The controller holds no tenant data
	sqlite3 *controller = turso_mock_open(m, "soju-controller");
	sqlite3_stmt *st = nullptr;
	assert_int_equal(sqlite3_prepare_v2(controller,
					    "SELECT count(*) FROM sqlite_master WHERE type='table' "
					    "AND name IN ('Message','Network','Channel')",
					    -1, &st, nullptr),
			 SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	assert_int_equal(sqlite3_column_int64(st, 0), 0);
	sqlite3_finalize(st);
	sqlite3_close(controller);
	db_close(db);

	db = turso_mock_database(m);
	irc_msg_vec v = list(db, a_net.id, "#a", nullptr);
	assert_int_equal(v.len, 1);
	assert_string_equal(irc_msg_trailing(v.data[0]), "alice's searchable message");
	irc_msg_vec_free(&v);
	v = list(db, b_net.id, "#a", nullptr);
	assert_string_equal(irc_msg_trailing(v.data[0]), "bob private");
	irc_msg_vec_free(&v);
	int64_t last = 0;
	assert_true(db_get_message_last_id(db, a_net.id, "#a", &last, nullptr));
	assert_int_equal(last, ids.data[0]);
	v = list(db, a_net.id, "#a", "searchable");
	assert_int_equal(v.len, 1);
	irc_msg_vec_free(&v);
	db_receipt_vec receipts = {0};
	assert_true(db_list_delivery_receipts(db, a_net.id, &receipts, nullptr));
	assert_string_equal(receipts.data[0].internal_msg_id, "cursor");
	db_receipt_vec_free(&receipts);
	assert_true(db_get_read_receipt(db, a_net.id, "#a", &read, &found, nullptr));
	assert_true(found);
	db_read_receipt_free(&read);
	struct db_stats stats;
	assert_true(db_stats(db, &stats, nullptr));
	assert_int_equal(stats.users, 2);
	assert_int_equal(stats.networks, 2);
	assert_true(db_delete_channel(db, channel.id, nullptr));
	size_t deleted = 0;
	struct timestamp before = timestamp_now();
	before.sec += 86400;
	assert_true(db_delete_messages_before(db, before, 10, &deleted, nullptr));
	assert_int_equal(deleted, 2);
	i64vec new_ids = {0};
	store(db, a_net.id, "#a", "again", &new_ids);
	assert_true(new_ids.data[0] > ids.data[0]);
	assert_true(db_optimize(db, nullptr));
	assert_true(db_delete_network(db, a_net.id, nullptr));
	assert_true(db_delete_user(db, alice.id, nullptr));
	assert_true(db_get_user(db, "alice", &auth, &found, nullptr));
	assert_false(found);
	db_network_vec nets = {0};
	assert_true(db_list_networks(db, bob.id, &nets, nullptr));
	assert_int_equal(nets.len, 1);
	db_network_vec_free(&nets);
	assert_int_equal(databases(m), 2);
	db_close(db);
	db = turso_mock_database(m);
	db_user_vec users = {0};
	assert_true(db_list_users(db, &users, nullptr));
	assert_int_equal(users.len, 1);
	db_user_vec_free(&users);
	db_close(db);
	vec_free(&ids);
	vec_free(&new_ids);
	db_channel_free(&channel);
	db_network_free(&a_net);
	db_network_free(&b_net);
	db_user_free(&alice);
	db_user_free(&bob);
	turso_mock_stop(m);
}

static void test_creation_and_deletion_resume_after_failure(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	m->st.fail_create = true;
	struct db_user alice;
	db_user_init(&alice, "alice");
	assert_true(db_user_set_password(&alice, "secret", nullptr));
	struct err *e = nullptr;
	assert_false(db_store_user(db, &alice, &e));
	err_free(e);
	struct db_user u;
	bool found = true;
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_false(found);
	db_user_vec users = {0};
	assert_true(db_list_users(db, &users, nullptr));
	assert_int_equal(users.len, 0);
	assert_int_equal(databases(m), 1);
	db_close(db);
	m->st.fail_create = false;
	db = turso_mock_database(m);
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_true(found);
	db_user_free(&u);
	assert_int_equal(databases(m), 2);
	m->st.fail_delete = true;
	e = nullptr;
	assert_false(db_delete_user(db, alice.id, &e));
	err_free(e);
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_false(found);
	db_close(db);
	m->st.fail_delete = false;
	db = turso_mock_database(m);
	assert_true(db_list_users(db, &users, nullptr));
	assert_int_equal(users.len, 0);
	assert_int_equal(databases(m), 1);
	db_close(db);
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void test_wrong_group_and_sql_failure_leave_no_partial_message(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice");
	struct db_network net = network(db, alice.id, "ircs://irc.example.org");
	struct irc_msg *msgs[2] = {IRC_MSG("PRIVMSG", "#a", "good"),
				   IRC_MSG("PRIVMSG", "#a", "bad")};
	irc_tags_set(&msgs[1]->tags, "time", "invalid time");
	i64vec ids = {0};
	struct err *e = nullptr;
	assert_false(db_store_messages(db, net.id, "#a", msgs, 2, &ids, &e));
	err_free(e);
	irc_msg_free(msgs[0]);
	irc_msg_free(msgs[1]);
	vec_free(&ids);
	int64_t last = -1;
	assert_true(db_get_message_last_id(db, net.id, "#a", &last, nullptr));
	assert_int_equal(last, 0);
	struct db_user duplicate;
	db_user_init(&duplicate, "alice");
	e = nullptr;
	assert_false(db_store_user(db, &duplicate, &e));
	err_free(e);
	db_user_free(&duplicate);
	assert_int_equal(databases(m), 2);
	db_close(db);
	m->st.bad_group = true;
	e = nullptr;
	assert_false(turso_mock_open_db(m, nullptr, &db, &e));
	assert_non_null(strstr(err_msg(e), "configured group"));
	err_free(e);
	db_network_free(&net);
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void test_names_retain_all_128_bits(void **state) {
	(void)state;
	struct strmap names;
	strmap_init(&names, nullptr);
	for (int i = 0; i < 1000; i++) {
		AUTOFREE char *name = turso_database_name();
		assert_int_equal(strlen(name), 25);
		assert_false(strmap_has(&names, name));
		strmap_set(&names, name, nullptr);
		// At most 2^128 - 1 in base 36
		assert_true(strcmp(name, "f5lxx1zz5pnorynqglhzmsp33") <= 0);
	}
	strmap_free(&names);
}

static void test_authorization_failures_do_not_expose_credentials(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct config_turso cfg = turso_mock_config();
	cfg.api_token = (char *)(uintptr_t)"wrong-api-secret";
	struct db *db = nullptr;
	struct err *e = nullptr;
	assert_false(turso_mock_open_db(m, &cfg, &db, &e));
	assert_non_null(strstr(err_msg(e), "401"));
	assert_null(strstr(err_msg(e), "wrong-api-secret"));
	err_free(e);
	cfg = turso_mock_config();
	cfg.group_token = (char *)(uintptr_t)"wrong-sql-secret";
	e = nullptr;
	assert_false(turso_mock_open_db(m, &cfg, &db, &e));
	assert_non_null(strstr(err_msg(e), "authorization failed"));
	assert_null(strstr(err_msg(e), "wrong-sql-secret"));
	err_free(e);
	turso_mock_stop(m);
}

static void test_missing_ready_database_is_not_recreated_empty(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice");
	AUTOFREE char *name = turso_mock_linked_name(db, alice.id);
	db_close(db);
	xfree(strmap_take(&m->st.databases, name));
	struct err *e = nullptr;
	assert_false(turso_mock_open_db(m, nullptr, &db, &e));
	assert_non_null(strstr(err_msg(e), "refusing to recreate"));
	err_free(e);
	assert_false(has_database(m, name));
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void test_invalid_links_and_changed_groups_cannot_delete_other_databases(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice");
	AUTOFREE char *name = turso_mock_linked_name(db, alice.id);
	assert_true(db_exec_i64(db->account,
				"UPDATE UserDatabase SET name = 'soju-controller' WHERE user = ?",
				alice.id, nullptr));
	struct err *e = nullptr;
	assert_false(db_delete_user(db, alice.id, &e));
	assert_non_null(strstr(err_msg(e), "invalid Turso database link"));
	err_free(e);
	assert_true(has_database(m, "soju-controller"));
	struct db_stmt *st =
		db_prepare(db->account, "UPDATE UserDatabase SET name = ? WHERE user = ?", nullptr);
	bind_pos_text(st, 1, name);
	bind_pos_i64(st, 2, alice.id);
	assert_true(db_step_done(st, nullptr));
	db_finalize(st);
	pthread_mutex_lock(&m->mu);
	strmap_set(&m->st.databases, name, xstrdup("other-group"));
	pthread_mutex_unlock(&m->mu);
	e = nullptr;
	assert_false(db_delete_user(db, alice.id, &e));
	assert_non_null(strstr(err_msg(e), "outside the configured group"));
	err_free(e);
	assert_true(has_database(m, name));
	struct db_user u;
	bool found = true;
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_false(found);
	db_close(db);
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void test_tenant_owner_mismatch_stops_startup(void **state) {
	(void)state;
	static const char *const sqls[] = {
		"UPDATE TursoOwner SET user = user + 1",
		"UPDATE TursoOwner SET controller = 'different-controller'",
	};
	for (size_t i = 0; i < 2; i++) {
		struct turso_mock *m = turso_mock_start();
		struct db *db = turso_mock_database(m);
		struct db_user alice = turso_mock_user(db, "alice");
		AUTOFREE char *name = turso_mock_linked_name(db, alice.id);
		db_close(db);
		sqlite3 *tenant = turso_mock_open(m, name);
		assert_int_equal(sqlite3_exec(tenant, sqls[i], nullptr, nullptr, nullptr),
				 SQLITE_OK);
		sqlite3_close(tenant);
		struct err *e = nullptr;
		assert_false(turso_mock_open_db(m, nullptr, &db, &e));
		err_free(e);
		assert_true(has_database(m, name));
		db_user_free(&alice);
		turso_mock_stop(m);
	}
}

static int64_t audit_count(struct db_conn *c) {
	int64_t n = -1;
	assert_true(db_query_i64(c, "SELECT count(*) FROM Audit", &n, nullptr));
	return n;
}

static void set_fault(struct turso_mock *m, const char *sql, enum turso_fault fault) {
	pthread_mutex_lock(&m->mu);
	xfree(m->st.fault_sql);
	m->st.fault_sql = xstrdup(sql);
	m->st.fault = fault;
	pthread_mutex_unlock(&m->mu);
}

static void test_lost_or_malformed_stream_never_continues_a_transaction(void **state) {
	(void)state;
	static const enum turso_fault faults[] = {
		TURSO_FAULT_LOST_STREAM, TURSO_FAULT_MISSING_RESULTS,
		TURSO_FAULT_WRONG_RESPONSE_TYPE, TURSO_FAULT_HTTP_AFTER, TURSO_FAULT_EMPTY_BODY};
	for (size_t i = 0; i < sizeof(faults) / sizeof(faults[0]); i++) {
		struct turso_mock *m = turso_mock_start();
		struct db *db = turso_mock_database(m);
		struct db_conn *c = db->account;
		assert_true(db_exec(c, "CREATE TABLE Audit(value INTEGER)", nullptr));
		assert_true(db_begin(c, nullptr));
		assert_true(db_exec_one(c, "INSERT INTO Audit VALUES (1)", nullptr));
		set_fault(m, "SELECT count(*) FROM Audit", faults[i]);
		int64_t n;
		struct err *e = nullptr;
		assert_false(db_query_i64(c, "SELECT count(*) FROM Audit", &n, &e));
		err_free(e);
		e = nullptr;
		assert_false(db_exec_one(c, "INSERT INTO Audit VALUES (2)", &e));
		err_free(e);
		db_rollback(c);
		assert_int_equal(audit_count(c), 0);
		assert_true(db_exec_one(c, "INSERT INTO Audit VALUES (3)", nullptr));
		assert_int_equal(audit_count(c), 1);
		db_close(db);
		turso_mock_stop(m);
	}
}

// The last pipeline request and its number of requests
static yyjson_doc *last_request(struct turso_mock *m) {
	pthread_mutex_lock(&m->mu);
	const char *last = m->st.requests.data[m->st.requests.len - 1];
	yyjson_doc *doc = yyjson_read(last, strlen(last), 0);
	pthread_mutex_unlock(&m->mu);
	return doc;
}

static void test_commit_closes_stream_on_hosted_and_legacy_servers(void **state) {
	(void)state;
	for (int hosted = 1; hosted >= 0; hosted--) {
		struct turso_mock *m = turso_mock_start();
		m->st.close_idle_streams = hosted != 0;
		struct db *db = turso_mock_database(m);
		struct db_conn *c = db->account;
		assert_true(db_exec(c, "CREATE TABLE Audit(value INTEGER)", nullptr));
		assert_true(db_begin(c, nullptr));
		assert_true(db_exec_one(c, "INSERT INTO Audit VALUES (1)", nullptr));
		assert_true(db_commit(c, nullptr));
		pthread_mutex_lock(&m->mu);
		assert_int_equal(strmap_len(&m->st.streams), 0);
		pthread_mutex_unlock(&m->mu);
		yyjson_doc *doc = last_request(m);
		yyjson_val *requests = yyjson_obj_get(yyjson_doc_get_root(doc), "requests");
		yyjson_val *first = yyjson_arr_get(requests, 0);
		assert_string_equal(
			yyjson_get_str(yyjson_obj_get(yyjson_obj_get(first, "stmt"), "sql")),
			"COMMIT");
		yyjson_val *last = yyjson_arr_get_last(requests);
		assert_string_equal(yyjson_get_str(yyjson_obj_get(last, "type")), "close");
		yyjson_doc_free(doc);
		assert_int_equal(audit_count(c), 1);
		struct db_user admin = turso_mock_user(db, "alice");
		db_close(db);
		db = turso_mock_database(m);
		struct db_user u;
		bool found = false;
		assert_true(db_get_user(db, "alice", &u, &found, nullptr));
		assert_int_equal(u.id, admin.id);
		db_user_free(&u);
		db_network_vec nets = {0};
		assert_true(db_list_networks(db, admin.id, &nets, nullptr));
		assert_int_equal(nets.len, 0);
		db_close(db);
		db_user_free(&admin);
		turso_mock_stop(m);
	}
}

static void test_failed_begin_releases_stream_and_ambiguous_commit_is_not_replayed(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_conn *c = db->account;
	assert_true(db_exec(c, "CREATE TABLE Audit(value INTEGER)", nullptr));
	set_fault(m, "BEGIN IMMEDIATE", TURSO_FAULT_MISSING_RESULTS);
	struct err *e = nullptr;
	assert_false(db_begin(c, &e));
	err_free(e);
	assert_true(db_begin(c, nullptr));
	assert_true(db_exec_one(c, "INSERT INTO Audit VALUES (1)", nullptr));
	set_fault(m, "COMMIT", TURSO_FAULT_HTTP_AFTER);
	e = nullptr;
	assert_false(db_commit(c, &e));
	err_free(e);
	db_rollback(c);
	assert_int_equal(audit_count(c), 1);
	assert_int_equal(turso_mock_count(m, "INSERT INTO Audit VALUES (1)"), 1);
	db_close(db);
	turso_mock_stop(m);
}

static void test_malformed_commit_responses_fail_without_replaying_writes(void **state) {
	(void)state;
	static const enum turso_fault faults[] = {TURSO_FAULT_MISSING_RESULTS,
						  TURSO_FAULT_WRONG_RESPONSE_TYPE,
						  TURSO_FAULT_EMPTY_BODY};
	for (size_t i = 0; i < 3; i++) {
		struct turso_mock *m = turso_mock_start();
		struct db *db = turso_mock_database(m);
		struct db_conn *c = db->account;
		assert_true(db_exec(c, "CREATE TABLE Audit(value INTEGER)", nullptr));
		assert_true(db_begin(c, nullptr));
		assert_true(db_exec_one(c, "INSERT INTO Audit VALUES (1)", nullptr));
		set_fault(m, "COMMIT", faults[i]);
		struct err *e = nullptr;
		assert_false(db_commit(c, &e));
		err_free(e);
		db_rollback(c);
		assert_int_equal(audit_count(c), 1);
		pthread_mutex_lock(&m->mu);
		assert_int_equal(strmap_len(&m->st.streams), 0);
		pthread_mutex_unlock(&m->mu);
		// Controller initialization and the Audit transaction
		assert_int_equal(turso_mock_count(m, "\"sql\":\"COMMIT\""), 2);
		db_close(db);
		turso_mock_stop(m);
	}
}

static void test_binary_values_named_parameters_and_replication_index(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_conn *c = db->account;
	assert_true(db_exec(c, "CREATE TABLE Audit(payload BLOB)", nullptr));
	char blob[64];
	memset(blob, 0xff, sizeof(blob));
	struct db_stmt *st = db_prepare(c, "INSERT INTO Audit(payload) VALUES (?)", nullptr);
	bind_pos_blob(st, 1, blob, sizeof(blob));
	assert_true(db_step_done(st, nullptr));
	db_finalize(st);
	uint64_t expected = m->st.replication_index;
	st = db_prepare(c, "SELECT payload FROM Audit", nullptr);
	assert_int_equal(db_step(st), SQLITE_ROW);
	struct str got = col_bytes(st, 0);
	assert_int_equal(got.len, sizeof(blob));
	assert_memory_equal(got.p, blob, sizeof(blob));
	db_finalize(st);
	yyjson_doc *doc = last_request(m);
	yyjson_val *req = yyjson_arr_get(yyjson_obj_get(yyjson_doc_get_root(doc), "requests"), 1);
	const char *index =
		yyjson_get_str(yyjson_obj_get(yyjson_obj_get(req, "stmt"), "replication_index"));
	char want[32];
	snprintf(want, sizeof(want), "%llu", (unsigned long long)expected);
	assert_string_equal(index, want);
	yyjson_doc_free(doc);
	st = db_prepare(c, "SELECT :value, @value, $value", nullptr);
	bind_i64(st, ":value", INT64_MAX);
	bind_i64(st, "@value", INT64_MIN);
	bind_i64(st, "$value", 42);
	assert_int_equal(db_step(st), SQLITE_ROW);
	assert_true(col_i64(st, 0) == INT64_MAX);
	assert_true(col_i64(st, 1) == INT64_MIN);
	assert_int_equal(col_i64(st, 2), 42);
	db_finalize(st);
	db_close(db);
	turso_mock_stop(m);
}

static void test_cross_tenant_network_update_cannot_poison_routes_after_restart(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice"), bob = turso_mock_user(db, "bob");
	struct db_network net = network(db, bob.id, "ircs://bob.example.org");
	store(db, net.id, "#private", "bob's history", nullptr);
	db_close(db);
	db = turso_mock_database(m); // the ownership cache starts empty
	xfree(net.addr);
	net.addr = xstrdup("ircs://wrong.example.org");
	struct err *e = nullptr;
	assert_false(db_store_network(db, alice.id, &net, &e));
	err_free(e);
	db_network_vec nets = {0};
	assert_true(db_list_networks(db, bob.id, &nets, nullptr));
	assert_string_equal(nets.data[0].addr, "ircs://bob.example.org");
	db_network_vec_free(&nets);
	irc_msg_vec v = list(db, net.id, "#private", nullptr);
	assert_string_equal(irc_msg_trailing(v.data[0]), "bob's history");
	irc_msg_vec_free(&v);
	db_close(db);
	db_network_free(&net);
	db_user_free(&alice);
	db_user_free(&bob);
	turso_mock_stop(m);
}

// clang-format off
T_MAIN(cmocka_unit_test(test_provisioning_authentication_isolation_and_restart),
       cmocka_unit_test(test_creation_and_deletion_resume_after_failure),
       cmocka_unit_test(test_wrong_group_and_sql_failure_leave_no_partial_message),
       cmocka_unit_test(test_names_retain_all_128_bits),
       cmocka_unit_test(test_authorization_failures_do_not_expose_credentials),
       cmocka_unit_test(test_missing_ready_database_is_not_recreated_empty),
       cmocka_unit_test(test_invalid_links_and_changed_groups_cannot_delete_other_databases),
       cmocka_unit_test(test_tenant_owner_mismatch_stops_startup),
       cmocka_unit_test(test_lost_or_malformed_stream_never_continues_a_transaction),
       cmocka_unit_test(test_commit_closes_stream_on_hosted_and_legacy_servers),
       cmocka_unit_test(test_failed_begin_releases_stream_and_ambiguous_commit_is_not_replayed),
       cmocka_unit_test(test_malformed_commit_responses_fail_without_replaying_writes),
       cmocka_unit_test(test_binary_values_named_parameters_and_replication_index),
       cmocka_unit_test(test_cross_tenant_network_update_cannot_poison_routes_after_restart))
// clang-format on
