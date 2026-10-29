// Ported from soju-rs src/database/turso/tests.rs: schema initialization
// and resumable migrations of Turso controllers and tenants
#include "testsupport.h"

#include <unistd.h>

#include "base/alloc.h"
#include "db/internal.h"
#include "turso_mock.h"

static const char SCHEMA_31[] = {
#embed "../fixtures/turso-schema-31.sql"
	, '\0'};

static int64_t raw_i64(sqlite3 *conn, const char *sql) {
	sqlite3_stmt *st = nullptr;
	assert_int_equal(sqlite3_prepare_v2(conn, sql, -1, &st, nullptr), SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	int64_t v = sqlite3_column_int64(st, 0);
	sqlite3_finalize(st);
	return v;
}

static void raw_exec(sqlite3 *conn, const char *sql) {
	char *e = nullptr;
	if (sqlite3_exec(conn, sql, nullptr, nullptr, &e) != SQLITE_OK) {
		fail_msg("%s: %s", sql, e);
	}
}

static char *path_of(struct turso_mock *m, const char *name) {
	return xasprintf("%s/%s", m->root, name);
}

static bool exists(sqlite3 *conn, const char *name) {
	sqlite3_stmt *st = nullptr;
	assert_int_equal(
		sqlite3_prepare_v2(conn, "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?)",
				   -1, &st, nullptr),
		SQLITE_OK);
	sqlite3_bind_text(st, 1, name, -1, SQLITE_TRANSIENT);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	bool v = sqlite3_column_int(st, 0) != 0;
	sqlite3_finalize(st);
	return v;
}

static void assert_tenant_message_layout(struct turso_mock *m, const char *name) {
	sqlite3 *conn = turso_mock_open(m, name);
	sqlite3_stmt *st = nullptr;
	assert_int_equal(sqlite3_prepare_v2(conn,
					    "SELECT sql FROM sqlite_master WHERE type='table' AND "
					    "name='Message'",
					    -1, &st, nullptr),
			 SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	const char *sql = (const char *)sqlite3_column_text(st, 0);
	assert_non_null(strstr(sql, "PRIMARY KEY(target, time, id)"));
	assert_non_null(strstr(sql, "WITHOUT ROWID"));
	sqlite3_finalize(st);
	assert_int_equal(sqlite3_prepare_v2(conn,
					    "SELECT name FROM sqlite_master WHERE type='index' AND "
					    "tbl_name='Message' AND sql IS NOT NULL",
					    -1, &st, nullptr),
			 SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	assert_string_equal((const char *)sqlite3_column_text(st, 0), "MessageTextIdIndex");
	assert_int_equal(sqlite3_step(st), SQLITE_DONE);
	sqlite3_finalize(st);
	static const char target[] =
		"(SELECT id FROM MessageTarget WHERE network=1 AND target='#a')";
	struct {
		char *query;
		const char *expected;
	} plans[] = {
		// Delivery cursors and search results seek text message IDs
		{xasprintf(
			 "SELECT id FROM Message WHERE target=%s AND text IS NOT NULL ORDER BY id "
			 "DESC LIMIT 1",
			 target),
		 "MessageTextIdIndex"},
		{xasprintf("SELECT time,id FROM Message WHERE target=%s AND text IS NOT NULL AND "
			   "id>5",
			   target),
		 "MessageTextIdIndex"},
		{xasprintf("SELECT m.id FROM (SELECT rowid AS r FROM MessageFTS WHERE MessageFTS "
			   "MATCH 'x') f CROSS JOIN Message m ON m.id=f.r WHERE m.target=%s AND "
			   "m.text IS NOT NULL",
			   target),
		 "MessageTextIdIndex"},
		// History, latest-target and expiry seek the clustered key
		{xasprintf(
			 "SELECT raw FROM Message m WHERE m.target=%s ORDER BY time DESC, id DESC "
			 "LIMIT 10",
			 target),
		 "PRIMARY KEY (target=?)"},
		{xstrdup("SELECT (SELECT time FROM Message WHERE target=MessageTarget.id ORDER BY "
			 "time DESC LIMIT 1) FROM MessageTarget"),
		 "PRIMARY KEY (target=?)"},
		{xstrdup("SELECT m.id FROM MessageTarget t CROSS JOIN Message m ON m.target=t.id "
			 "WHERE m.time<'x' LIMIT 10"),
		 "PRIMARY KEY (target=? AND time<?)"},
	};
	for (size_t i = 0; i < sizeof(plans) / sizeof(plans[0]); i++) {
		AUTOFREE char *q = xasprintf("EXPLAIN QUERY PLAN %s", plans[i].query);
		assert_int_equal(sqlite3_prepare_v2(conn, q, -1, &st, nullptr), SQLITE_OK);
		bool found = false;
		while (sqlite3_step(st) == SQLITE_ROW) {
			const char *detail = (const char *)sqlite3_column_text(st, 3);
			found = found || strstr(detail, plans[i].expected) != nullptr;
			assert_null(strstr(detail, "TEMP B-TREE"));
		}
		sqlite3_finalize(st);
		if (!found) {
			fail_msg("%s: no %s", plans[i].query, plans[i].expected);
		}
		xfree(plans[i].query);
	}
	sqlite3_close(conn);
}

static struct db_network network(struct db *db, int64_t user_id) {
	struct db_network n;
	db_network_init(&n, "ircs://irc.example.org");
	assert_true(db_store_network(db, user_id, &n, nullptr));
	return n;
}

static void test_user_schema_migration_preserves_messages(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice");
	struct db_network net = network(db, alice.id);
	struct irc_msg *msg = IRC_MSG("PRIVMSG", "#a", "old message");
	i64vec ids = {0};
	assert_true(db_store_messages(db, net.id, "#a", &msg, 1, &ids, nullptr));
	vec_free(&ids);
	AUTOFREE char *name = turso_mock_linked_name(db, alice.id);
	db_close(db);
	AUTOFREE char *path = path_of(m, name);
	unlink(path);
	sqlite3 *conn = turso_mock_open(m, name);
	raw_exec(conn, SCHEMA_31);
	AUTOFREE char *raw = irc_msg_to_string(msg);
	AUTOFREE char *setup =
		xasprintf("INSERT INTO User(id,username,created_at) VALUES(%lld,'user-%lld','');"
			  "INSERT INTO Network(id,user,addr) VALUES(%lld,%lld,'%s');"
			  "INSERT INTO MessageTarget(id,network,target) VALUES(1,%lld,'#a');"
			  "INSERT INTO Message(id,target,raw,time,sender,text) "
			  "VALUES(1,1,'%s','2024-01-01T00:00:00.000Z','','old message');"
			  "DROP INDEX MessageTextIndex; PRAGMA user_version = 30;",
			  (long long)alice.id, (long long)alice.id, (long long)net.id,
			  (long long)alice.id, net.addr, (long long)net.id, raw);
	raw_exec(conn, setup);
	sqlite3_close(conn);
	db = turso_mock_database(m);
	struct db_message_options o = {.limit = 10};
	irc_msg_vec v = {0};
	assert_true(db_list_messages(db, net.id, "#a", &o, &v, nullptr));
	assert_int_equal(v.len, 1);
	assert_msg_str(v.data[0], raw);
	irc_msg_vec_free(&v);
	db_close(db);
	assert_tenant_message_layout(m, name);
	irc_msg_free(msg);
	db_network_free(&net);
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void test_initialize_an_existing_empty_controller_with_read_only_user_version(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	strmap_set(&m->st.databases, "soju-controller", xstrdup("soju"));
	sqlite3_close(turso_mock_open(m, "soju-controller"));
	struct db *db = turso_mock_database(m);
	int64_t version = 0;
	assert_true(db_query_i64(db->account, "SELECT version FROM SojuSchemaVersion WHERE id=0",
				 &version, nullptr));
	assert_int_equal(version, 2);
	struct db_user admin = turso_mock_user(db, "alice");
	struct db_user u;
	bool found = false;
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_true(found && u.admin);
	db_user_free(&u);
	db_network_vec nets = {0};
	assert_true(db_list_networks(db, admin.id, &nets, nullptr));
	assert_int_equal(nets.len, 0);
	assert_int_equal(turso_mock_count(m, "PRAGMA user_version ="), 0);
	assert_int_equal(turso_mock_count(m, "pragma user_version ="), 0);
	db_close(db);
	db_user_free(&admin);
	turso_mock_stop(m);
}

static void
test_existing_legacy_controller_imports_its_schema_version_and_keeps_accounts(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user alice = turso_mock_user(db, "alice");
	db_close(db);
	sqlite3 *conn = turso_mock_open(m, "soju-controller");
	raw_exec(conn, "DROP TABLE SojuSchemaVersion; PRAGMA user_version = 1;");
	sqlite3_close(conn);
	db = turso_mock_database(m);
	struct db_user u;
	bool found = false;
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_true(found && u.admin);
	db_user_free(&u);
	int64_t version = 0;
	assert_true(db_query_i64(db->account, "SELECT version FROM SojuSchemaVersion WHERE id=0",
				 &version, nullptr));
	assert_int_equal(version, 2);
	db_close(db);
	db_user_free(&alice);
	turso_mock_stop(m);
}

static void set_fault(struct turso_mock *m, const char *sql) {
	pthread_mutex_lock(&m->mu);
	xfree(m->st.fault_sql);
	m->st.fault_sql = xstrdup(sql);
	m->st.fault = TURSO_FAULT_HTTP_AFTER;
	pthread_mutex_unlock(&m->mu);
}

static size_t count_messages(struct db *db, bool events, const char *text) {
	struct db_message_options o = {.limit = 5000, .events = events, .text = text};
	irc_msg_vec v = {0};
	assert_true(db_list_messages(db, 1, "#legacy", &o, &v, nullptr));
	size_t n = v.len;
	irc_msg_vec_free(&v);
	return n;
}

static void test_tenant_user_removal_and_sparse_search_resume_preserve_existing_data(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct db_user account = turso_mock_user(db, "alice");
	AUTOFREE char *name = turso_mock_linked_name(db, account.id);
	db_close(db);
	AUTOFREE char *path = path_of(m, name);
	unlink(path);
	sqlite3 *conn = turso_mock_open(m, name);
	raw_exec(conn, SCHEMA_31);
	long long id = (long long)account.id;
	AUTOFREE char *setup = xasprintf(
		"INSERT INTO User(id,username,created_at)VALUES(%lld,'user-%lld','');"
		"INSERT INTO Network(id,user,addr,pass,sasl_mechanism,sasl_plain_username,"
		"sasl_plain_password)VALUES(1,%lld,'irc.legacy','network-secret','PLAIN','login',"
		"'sasl-secret');"
		"INSERT INTO WebPushSubscription(id,created_at,updated_at,user,network,endpoint,"
		"key_auth)VALUES(1,'2024-01-01T00:00:00.000Z','2024-01-01T00:00:00.000Z',%lld,1,"
		"'https://push.example','push-secret');"
		"INSERT INTO Channel(id,network,name,key)VALUES(1,1,'#legacy','channel-secret');"
		"INSERT INTO MessageTarget(id,network,target,pinned)VALUES(1,1,'#legacy',1),(2,1,"
		"'#events',0);"
		"WITH RECURSIVE m(id)AS(VALUES(1) UNION ALL SELECT id+1 FROM m WHERE id<2400)"
		"INSERT INTO Message(id,target,raw,time,sender,text)SELECT id,1,"
		"CASE WHEN id%%3=0 THEN 'JOIN #legacy' ELSE 'PRIVMSG #legacy :legacy token' END,"
		"'2024-01-01T00:00:00.000Z','',CASE WHEN id%%3!=0 THEN 'legacy token' END FROM m;"
		"INSERT INTO Message(id,target,raw,time,sender,text)VALUES(2401,2,'JOIN #events',"
		"'2024-01-01T00:00:00.000Z','',NULL);"
		"CREATE TABLE MessageIdHighWater(id INTEGER PRIMARY KEY CHECK(id=0),last_id "
		"INTEGER "
		"NOT NULL);"
		"INSERT INTO MessageIdHighWater(id,last_id)VALUES(0,7);"
		"INSERT INTO DeliveryReceipt(network,target,client,internal_msgid)VALUES(1,"
		"'#legacy',NULL,'cursor');"
		"INSERT INTO ReadReceipt(network,target,timestamp)VALUES(1,'#legacy',"
		"'2024-01-01T00:00:00.000Z');"
		"PRAGMA user_version=31;",
		id, id, id, id);
	raw_exec(conn, setup);
	sqlite3_close(conn);
	// The first chunk commits, but its response is lost
	set_fault(m, "INSERT INTO MessageNext");
	struct err *e = nullptr;
	assert_false(turso_mock_open_db(m, nullptr, &db, &e));
	err_free(e);
	conn = turso_mock_open(m, name);
	assert_int_equal(raw_i64(conn, "SELECT message FROM TursoMessageRebuild"), 1000);
	assert_int_equal(raw_i64(conn, "SELECT count(*) FROM MessageNext"), 1000);
	assert_int_equal(raw_i64(conn, "SELECT last_id FROM MessageIdHighWater"), 2401);
	sqlite3_close(conn);
	set_fault(m, "INSERT INTO MessageFTSNext");
	e = nullptr;
	assert_false(turso_mock_open_db(m, nullptr, &db, &e));
	err_free(e);
	conn = turso_mock_open(m, name);
	assert_true(raw_i64(conn, "SELECT message FROM TursoSearchMigration WHERE id=0") > 0);
	assert_true(exists(conn, "User"));
	sqlite3_close(conn);

	db = turso_mock_database(m);
	struct db_user u;
	bool found = false;
	assert_true(db_get_user(db, "alice", &u, &found, nullptr));
	assert_true(found && u.admin);
	db_user_free(&u);
	db_network_vec nets = {0};
	assert_true(db_list_networks(db, account.id, &nets, nullptr));
	assert_string_equal(nets.data[0].pass, "network-secret");
	assert_string_equal(nets.data[0].sasl.plain_password, "sasl-secret");
	db_network_vec_free(&nets);
	db_channel_vec chans = {0};
	assert_true(db_list_channels(db, 1, &chans, nullptr));
	assert_string_equal(chans.data[0].key, "channel-secret");
	db_channel_vec_free(&chans);
	db_webpush_sub_vec subs = {0};
	assert_true(db_list_webpush_subscriptions(db, account.id, 1, &subs, nullptr));
	assert_string_equal(subs.data[0].key_auth, "push-secret");
	db_webpush_sub_vec_free(&subs);
	db_receipt_vec receipts = {0};
	assert_true(db_list_delivery_receipts(db, 1, &receipts, nullptr));
	assert_string_equal(receipts.data[0].internal_msg_id, "cursor");
	db_receipt_vec_free(&receipts);
	struct db_read_receipt read;
	assert_true(db_get_read_receipt(db, 1, "#legacy", &read, &found, nullptr));
	assert_true(found);
	db_read_receipt_free(&read);
	assert_int_equal(count_messages(db, false, "legacy"), 1600);
	assert_int_equal(count_messages(db, true, nullptr), 2400);
	// Reserved IDs continue after the legacy messages, not the stale row
	struct irc_msg *msg = IRC_MSG("PRIVMSG", "#legacy", "new");
	i64vec ids = {0};
	assert_true(db_store_messages(db, 1, "#legacy", &msg, 1, &ids, nullptr));
	irc_msg_free(msg);
	assert_int_equal(ids.data[0], 2402);
	int64_t last = 0;
	assert_true(db_get_message_last_id(db, 1, "#legacy", &last, nullptr));
	assert_int_equal(last, 2402);
	vec_free(&ids);
	conn = turso_mock_open(m, name);
	static const char *const gone[] = {
		"User",
		"DeviceCertificate",
		"WebPushConfig",
		"TursoSearchMigration",
		"MessageFTSNext",
		"TursoMessageRebuild",
		"MessageNext",
		"MessageTextIndex",
		"MessageEventIndex",
		"Message_target_index",
		"MessageFTS_docsize",
	};
	for (size_t i = 0; i < sizeof(gone) / sizeof(gone[0]); i++) {
		if (exists(conn, gone[i])) {
			fail_msg("%s survived migration", gone[i]);
		}
	}
	sqlite3_stmt *st = nullptr;
	assert_int_equal(sqlite3_prepare_v2(conn, "PRAGMA foreign_key_check", -1, &st, nullptr),
			 SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_DONE);
	sqlite3_finalize(st);
	raw_exec(conn, "INSERT INTO MessageFTS(MessageFTS,rank)VALUES('integrity-check',1)");
	raw_exec(conn, "PRAGMA foreign_keys=ON;");
	assert_int_not_equal(sqlite3_exec(conn, "UPDATE Network SET user=user+1 WHERE id=1",
					  nullptr, nullptr, nullptr),
			     SQLITE_OK);
	sqlite3_close(conn);
	db_close(db);
	db = turso_mock_database(m);
	db_close(db);
	db_user_free(&account);
	turso_mock_stop(m);
}

// clang-format off
T_MAIN(cmocka_unit_test(test_user_schema_migration_preserves_messages),
       cmocka_unit_test(test_initialize_an_existing_empty_controller_with_read_only_user_version),
       cmocka_unit_test(test_existing_legacy_controller_imports_its_schema_version_and_keeps_accounts),
       cmocka_unit_test(test_tenant_user_removal_and_sparse_search_resume_preserve_existing_data))
// clang-format on
