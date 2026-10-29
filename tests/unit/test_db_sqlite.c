// Ported from soju-rs src/database/sqlite/tests.rs and the password test
// of src/database/mod.rs
#include "testsupport.h"

#include <crypt.h>
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

#include "base/alloc.h"
#include "db/db.h"

static struct db *open_db(void) {
	struct db *db = nullptr;
	struct err *e = nullptr;
	if (!db_open_in_memory(&db, &e)) {
		fail_msg("%s", err_msg(e));
	}
	return db;
}

static void ok(bool b, struct err *e) {
	if (!b) {
		fail_msg("%s", err_msg(e));
	}
}

#define OK(call)                          \
	do {                              \
		struct err *e_ = nullptr; \
		ok((call), e_);           \
	} while (0)

// Calls with &e_ as their last argument
#define T(f, ...)                            \
	do {                                 \
		struct err *e_ = nullptr;    \
		ok(f(__VA_ARGS__, &e_), e_); \
	} while (0)

static void store_user(struct db *db, struct db_user *u, const char *name) {
	db_user_init(u, name);
	T(db_store_user, db, u);
}

static void store_network(struct db *db, int64_t user, struct db_network *n, const char *addr) {
	db_network_init(n, addr);
	T(db_store_network, db, user, n);
}

static struct db_time at(const char *s) {
	struct db_time t = {.set = true};
	assert_true(parse_server_time(str_from(s), &t.t));
	return t;
}

static struct irc_msg *msg_at(const char *line, const char *time) {
	struct irc_msg *m = irc_msg_parse(str_from(line), nullptr);
	assert_non_null(m);
	if (time != nullptr) {
		irc_tags_set(&m->tags, "time", time);
	}
	return m;
}

static void assert_msgs(struct db *db, int64_t net, const char *target,
			const struct db_message_options *o, const char *const *want, size_t n) {
	irc_msg_vec l = {0};
	T(db_list_messages, db, net, target, o, &l);
	if (l.len != n) {
		fail_msg("got %zu messages, want %zu", l.len, n);
	}
	for (size_t i = 0; i < n; i++) {
		assert_msg_str(l.data[i], want[i]);
	}
	irc_msg_vec_free(&l);
}

static size_t count_msgs(struct db *db, int64_t net, const char *target,
			 const struct db_message_options *o) {
	irc_msg_vec l = {0};
	T(db_list_messages, db, net, target, o, &l);
	size_t n = l.len;
	irc_msg_vec_free(&l);
	return n;
}

static void test_users(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	db_user_init(&u, "alice");
	T(db_user_set_password, &u, "pw");
	T(db_store_user, db, &u);
	assert_int_not_equal(u.id, 0);
	struct db_user got;
	bool found;
	T(db_get_user, db, "alice", &got, &found);
	assert_true(found);
	assert_int_equal(got.id, u.id);
	assert_string_equal(got.password, u.password);
	assert_true(got.enabled && !got.admin && got.max_networks == -1);
	xfree(got.nick);
	got.nick = xstrdup("Alice");
	got.downstream_interacted_at = (struct db_time){.set = true, .t = timestamp_now()};
	T(db_store_user, db, &got);
	db_user_vec users = {0};
	T(db_list_users, db, &users);
	assert_int_equal(users.len, 1);
	assert_string_equal(users.data[0].nick, "Alice");
	assert_true(users.data[0].downstream_interacted_at.set);
	db_user_vec_free(&users);
	char *name;
	T(db_get_username_by_id, db, u.id, &name);
	assert_string_equal(name, "alice");
	xfree(name);
	struct db_user none;
	T(db_get_user, db, "bob", &none, &found);
	assert_false(found);

	struct timestamp later = timestamp_now(), earlier = later;
	later.sec += 3600;
	earlier.sec -= 3600;
	strvec inactive = {0};
	T(db_list_inactive_usernames, db, later, &inactive);
	assert_int_equal(inactive.len, 1);
	assert_string_equal(inactive.data[0], "alice");
	strvec_free(&inactive);
	// Disabled users are already inactive
	got.enabled = false;
	T(db_store_user, db, &got);
	T(db_list_inactive_usernames, db, later, &inactive);
	assert_int_equal(inactive.len, 0);
	T(db_list_inactive_usernames, db, earlier, &inactive);
	assert_int_equal(inactive.len, 0);
	T(db_delete_user, db, u.id);
	T(db_list_users, db, &users);
	assert_int_equal(users.len, 0);
	db_user_free(&got);
	db_user_free(&u);
	db_close(db);
}

static void test_networks_channels(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	db_network_init(&n, "irc.example.org");
	strvec_push(&n.connect_commands, "PRIVMSG a :b");
	strvec_push(&n.connect_commands, "JOIN #x");
	xfree(n.sasl.mechanism);
	n.sasl.mechanism = xstrdup("EXTERNAL");
	n.sasl.external_cert.set = true;
	strbuf_append(&n.sasl.external_cert.data, "\1\2\3", 3);
	n.sasl.external_key.set = true;
	strbuf_append(&n.sasl.external_key.data, "\4", 1);
	T(db_store_network, db, u.id, &n);
	db_network_vec nets = {0};
	T(db_list_networks, db, u.id, &nets);
	assert_int_equal(nets.len, 1);
	struct db_network *g = &nets.data[0];
	assert_int_equal(g->id, n.id);
	assert_string_equal(g->addr, "irc.example.org");
	assert_int_equal(g->connect_commands.len, 2);
	assert_string_equal(g->connect_commands.data[0], "PRIVMSG a :b");
	assert_string_equal(g->connect_commands.data[1], "JOIN #x");
	assert_string_equal(g->sasl.mechanism, "EXTERNAL");
	assert_true(g->sasl.external_cert.set && g->sasl.external_cert.data.len == 3);
	assert_memory_equal(g->sasl.external_key.data.buf, "\4", 1);
	assert_true(g->auto_away && g->enabled);
	db_network_vec_free(&nets);

	xfree(n.sasl.mechanism);
	n.sasl.mechanism = xstrdup("PLAIN");
	xfree(n.sasl.plain_username);
	n.sasl.plain_username = xstrdup("acc");
	T(db_store_network, db, u.id, &n);
	T(db_list_networks, db, u.id, &nets);
	assert_false(nets.data[0].sasl.external_cert.set);
	assert_string_equal(nets.data[0].sasl.plain_username, "acc");
	db_network_vec_free(&nets);

	struct db_channel ch;
	db_channel_init(&ch);
	xfree(ch.name);
	ch.name = xstrdup("#soju");
	ch.detach_after_ns = 1500000000;
	ch.relay_detached = FILTER_MESSAGE;
	T(db_store_channel, db, n.id, &ch);
	db_channel_vec chs = {0};
	T(db_list_channels, db, n.id, &chs);
	assert_int_equal(chs.len, 1);
	assert_int_equal(chs.data[0].detach_after_ns, 2000000000);
	assert_int_equal(chs.data[0].relay_detached, FILTER_MESSAGE);
	db_channel_vec_free(&chs);
	T(db_delete_channel, db, ch.id);
	T(db_list_channels, db, n.id, &chs);
	assert_int_equal(chs.len, 0);
	T(db_delete_network, db, n.id);
	T(db_list_networks, db, u.id, &nets);
	assert_int_equal(nets.len, 0);
	db_channel_free(&ch);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static void store_msgs(struct db *db, int64_t net, const char *target, struct irc_msg *const *m,
		       size_t n, i64vec *ids) {
	T(db_store_messages, db, net, target, m, n, ids);
}

static void test_messages(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");

	struct irc_msg *m[3] = {
		msg_at(":bob PRIVMSG #a :hello \x02world\x02", "2024-01-01T00:00:00.000Z"),
		msg_at(":carol JOIN #a", "2024-01-01T00:00:01.000Z"),
		msg_at(":bob NOTICE #a :goodbye world", "2024-01-01T00:00:02.000Z"),
	};
	char *s[3];
	for (int i = 0; i < 3; i++) {
		s[i] = irc_msg_to_string(m[i]);
	}
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", m, 3, &ids);
	assert_int_equal(ids.len, 3);
	int64_t last;
	T(db_get_message_last_id, db, n.id, "#a", &last);
	assert_int_equal(last, ids.data[2]);
	T(db_get_message_last_id, db, n.id, "#b", &last);
	assert_int_equal(last, 0);

	struct db_message_options o = {.limit = 10};
	assert_msgs(db, n.id, "#a", &o, (const char *[]){s[0], s[2]}, 2);
	o = (struct db_message_options){.limit = 10, .events = true};
	assert_int_equal(count_msgs(db, n.id, "#a", &o), 3);
	o = (struct db_message_options){.limit = 1, .take_last = true};
	assert_msgs(db, n.id, "#a", &o, (const char *[]){s[2]}, 1);
	o = (struct db_message_options){.limit = 10, .text = "world\""};
	assert_int_equal(count_msgs(db, n.id, "#a", &o), 2);
	// Nothing to search for, rather than an FTS syntax error
	o = (struct db_message_options){.limit = 10, .text = "!!!"};
	assert_int_equal(count_msgs(db, n.id, "#a", &o), 0);
	o = (struct db_message_options){.limit = 10, .text = "hello world", .sender = "bob"};
	assert_msgs(db, n.id, "#a", &o, (const char *[]){s[0]}, 1);
	o = (struct db_message_options){.limit = 10, .after_time = at("2024-01-01T00:00:00.000Z")};
	assert_msgs(db, n.id, "#a", &o, (const char *[]){s[2]}, 1);

	struct irc_msg *mb = msg_at("PRIVMSG #b x", "2024-01-01T00:00:05.000Z");
	store_msgs(db, n.id, "#b", &mb, 1, &ids);
	irc_msg_free(mb);
	o = (struct db_message_options){
		.limit = 10,
		.after_time = at("2023-01-01T00:00:00.000Z"),
		.before_time = at("2025-01-01T00:00:00.000Z"),
	};
	db_target_last_vec targets = {0};
	T(db_list_message_last_per_target, db, n.id, &o, &targets);
	assert_int_equal(targets.len, 2);
	assert_string_equal(targets.data[0].name, "#a");
	assert_string_equal(targets.data[1].name, "#b");
	struct db_time want = at("2024-01-01T00:00:02.000Z");
	assert_true(targets.data[0].latest_message.set);
	assert_int_equal(timestamp_cmp(targets.data[0].latest_message.t, want.t), 0);
	db_target_last_vec_free(&targets);

	size_t deleted;
	T(db_delete_messages_before, db, at("2024-01-01T00:00:01.500Z").t, SIZE_MAX, &deleted);
	o = (struct db_message_options){.limit = 10, .events = true};
	assert_msgs(db, n.id, "#a", &o, (const char *[]){s[2]}, 1);

	for (int i = 0; i < 3; i++) {
		irc_msg_free(m[i]);
		xfree(s[i]);
	}
	vec_free(&ids);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static void texts_eq(struct db *db, int64_t net, const struct db_message_options *o,
		     const char *const *want, size_t n) {
	irc_msg_vec l = {0};
	T(db_list_messages, db, net, "#a", o, &l);
	assert_int_equal(l.len, n);
	for (size_t i = 0; i < n; i++) {
		assert_string_equal(irc_msg_param(l.data[i], 1), want[i]);
	}
	irc_msg_vec_free(&l);
}

static void test_messages_same_time(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	// e.g. the lines of a multiline message
	struct irc_msg *m[5];
	for (int i = 0; i < 5; i++) {
		char line[32];
		snprintf(line, sizeof(line), "PRIVMSG #a %d", i);
		m[i] = msg_at(line, "2024-01-01T00:00:00.000Z");
	}
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", m, 5, &ids);
	int64_t last;
	T(db_get_message_last_id, db, n.id, "#a", &last);
	assert_int_equal(last, ids.data[4]);
	struct db_message_options o = {.limit = 10};
	texts_eq(db, n.id, &o, (const char *[]){"0", "1", "2", "3", "4"}, 5);
	o = (struct db_message_options){.limit = 2, .take_last = true};
	texts_eq(db, n.id, &o, (const char *[]){"3", "4"}, 2);
	o = (struct db_message_options){.after_id = ids.data[2], .limit = 10, .take_last = true};
	texts_eq(db, n.id, &o, (const char *[]){"3", "4"}, 2);
	for (int i = 0; i < 5; i++) {
		irc_msg_free(m[i]);
	}
	vec_free(&ids);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static int64_t store_one(struct db *db, int64_t net, struct irc_msg *m) {
	i64vec ids = {0};
	store_msgs(db, net, "#a", &m, 1, &ids);
	int64_t id = ids.data[0];
	vec_free(&ids);
	return id;
}

static void test_message_ids_not_reused(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	struct irc_msg *m = msg_at("PRIVMSG #a x", "2024-01-01T00:00:00.000Z");

	// Expiry empties the table
	int64_t id1 = store_one(db, n.id, m);
	size_t deleted;
	T(db_delete_messages_before, db, timestamp_now(), SIZE_MAX, &deleted);
	assert_int_equal(deleted, 1);
	int64_t id2 = store_one(db, n.id, m);
	assert_true(id2 > id1);
	struct db_message_options o = {.after_id = id1, .limit = 10};
	assert_int_equal(count_msgs(db, n.id, "#a", &o), 1);

	// Deleting a network or a user removes the highest IDs
	struct db_network n2;
	store_network(db, u.id, &n2, "irc.example.com");
	int64_t id3 = store_one(db, n2.id, m);
	T(db_delete_network, db, n2.id);
	int64_t id4 = store_one(db, n.id, m);
	assert_true(id4 > id3);
	struct db_user u2;
	store_user(db, &u2, "bob");
	struct db_network n3;
	store_network(db, u2.id, &n3, "irc.example.org");
	int64_t id5 = store_one(db, n3.id, m);
	T(db_delete_user, db, u2.id);
	int64_t id6 = store_one(db, n.id, m);
	assert_true(id6 > id5);
	irc_msg_free(m);
	db_network_free(&n);
	db_network_free(&n2);
	db_network_free(&n3);
	db_user_free(&u);
	db_user_free(&u2);
	db_close(db);
}

static void test_delete_messages_in_chunks(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	struct irc_msg *m[6];
	for (int i = 0; i < 5; i++) {
		m[i] = msg_at("PRIVMSG #a x", "2024-01-01T00:00:00.000Z");
	}
	m[5] = msg_at("PRIVMSG #a y", "2030-01-01T00:00:00.000Z");
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", m, 6, &ids);
	struct timestamp before = at("2025-01-01T00:00:00.000Z").t;
	const size_t want[] = {2, 2, 1, 0};
	for (size_t i = 0; i < 4; i++) {
		size_t d;
		T(db_delete_messages_before, db, before, 2, &d);
		assert_int_equal(d, want[i]);
	}
	struct db_message_options o = {.limit = 10};
	assert_int_equal(count_msgs(db, n.id, "#a", &o), 1);
	for (int i = 0; i < 6; i++) {
		irc_msg_free(m[i]);
	}
	vec_free(&ids);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static void test_message_last_id_insertion_order(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	// The upstream server's clock went backwards
	struct irc_msg *m[2] = {
		msg_at("PRIVMSG #a x", "2030-01-01T00:00:00.000Z"),
		msg_at("PRIVMSG #a y", "2024-01-01T00:00:00.000Z"),
	};
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", m, 2, &ids);
	int64_t last;
	T(db_get_message_last_id, db, n.id, "#a", &last);
	assert_int_equal(last, ids.data[1]);
	irc_msg_free(m[0]);
	irc_msg_free(m[1]);
	vec_free(&ids);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static off_t file_size(const char *path) {
	struct stat stbuf;
	return stat(path, &stbuf) == 0 ? stbuf.st_size : 0;
}

static void test_delete_messages_vacuum(void **state) {
	(void)state;
	char dir[] = "/tmp/soju-c-vacuum-XXXXXX";
	assert_non_null(mkdtemp(dir));
	char path[128], wal[128];
	snprintf(path, sizeof(path), "%s/main.db", dir);
	snprintf(wal, sizeof(wal), "%s/main.db-wal", dir);
	struct db *db = nullptr;
	T(db_open, path, &db);
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	struct strbuf line = STRBUF_INIT;
	strbuf_append_cstr(&line, "PRIVMSG #a :");
	for (int i = 0; i < 1000; i++) {
		strbuf_append_char(&line, 'x');
	}
	struct irc_msg **m = xcalloc(2000, sizeof(*m));
	for (int i = 0; i < 2000; i++) {
		m[i] = msg_at(strbuf_cstr(&line), "2024-01-01T00:00:00.000Z");
	}
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", m, 2000, &ids);
	assert_int_equal(sqlite3_wal_checkpoint_v2(db_handle(db), nullptr,
						   SQLITE_CHECKPOINT_TRUNCATE, nullptr, nullptr),
			 SQLITE_OK);
	off_t before = file_size(path);
	double ratio;
	T(db_free_page_ratio, db, &ratio);
	assert_true(ratio < 0.05);
	size_t deleted;
	T(db_delete_messages_before, db, timestamp_now(), SIZE_MAX, &deleted);
	assert_int_equal(deleted, 2000);
	T(db_free_page_ratio, db, &ratio);
	assert_true(ratio > 0.5);
	T(db_optimize, db);
	T(db_vacuum, db);
	T(db_shrink_memory, db);
	T(db_free_page_ratio, db, &ratio);
	assert_true(ratio == 0.0);
	assert_true(file_size(path) < before / 2);
	assert_int_equal(file_size(wal), 0);
	for (int i = 0; i < 2000; i++) {
		irc_msg_free(m[i]);
	}
	xfree(m);
	strbuf_free(&line);
	vec_free(&ids);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
	unlink(path);
	unlink(wal);
	char shm[128];
	snprintf(shm, sizeof(shm), "%s/main.db-shm", dir);
	unlink(shm);
	rmdir(dir);
}

static void exec_blob(struct db *db, const char *sql, int64_t i1, const void *b, int n) {
	sqlite3_stmt *st;
	assert_int_equal(sqlite3_prepare_v2(db_handle(db), sql, -1, &st, nullptr), SQLITE_OK);
	if (i1 != 0) {
		sqlite3_bind_int64(st, 1, i1);
		sqlite3_bind_blob(st, 2, b, n, SQLITE_TRANSIENT);
	} else {
		sqlite3_bind_blob(st, 1, b, n, SQLITE_TRANSIENT);
	}
	assert_int_equal(sqlite3_step(st), SQLITE_DONE);
	sqlite3_finalize(st);
}

static void test_messages_invalid_utf8(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");
	struct irc_msg *m = msg_at("PRIVMSG #a x", nullptr);
	i64vec ids = {0};
	store_msgs(db, n.id, "#a", &m, 1, &ids);
	irc_msg_free(m);
	vec_free(&ids);

	// Go soju stores raw upstream bytes, which may be e.g. Latin-1
	static const char raw[] = "@time=2024-01-01T00:00:00.000Z :bob PRIVMSG #a :caf\xe9";
	exec_blob(db,
		  "INSERT INTO Message(target, raw, time, sender, text)\n"
		  "            SELECT id, CAST(?1 AS TEXT), '2024-01-01T00:00:00.000Z', 'bob', "
		  "'caf'\n"
		  "            FROM MessageTarget WHERE target = '#a'",
		  0, raw, (int)sizeof(raw) - 1);
	struct db_message_options o = {.limit = 10};
	irc_msg_vec l = {0};
	T(db_list_messages, db, n.id, "#a", &o, &l);
	assert_int_equal(l.len, 2);
	assert_string_equal(irc_msg_param(l.data[0], 0), "#a");
	assert_string_equal(irc_msg_param(l.data[0], 1), "caf\xEF\xBF\xBD");
	irc_msg_vec_free(&l);

	// Channel names and delivery receipt targets come from upstream too
	static const char name[] = "#caf\xe9";
	exec_blob(db, "INSERT INTO Channel(network, name) VALUES (?1, CAST(?2 AS TEXT))", n.id,
		  name, (int)sizeof(name) - 1);
	exec_blob(db,
		  "INSERT INTO DeliveryReceipt(network, target, internal_msgid)\n"
		  "            VALUES (?1, CAST(?2 AS TEXT), 'x')",
		  n.id, name, (int)sizeof(name) - 1);
	db_channel_vec chs = {0};
	T(db_list_channels, db, n.id, &chs);
	assert_string_equal(chs.data[0].name, "#caf\xEF\xBF\xBD");
	db_channel_vec_free(&chs);
	db_receipt_vec rs = {0};
	T(db_list_delivery_receipts, db, n.id, &rs);
	assert_string_equal(rs.data[0].target, "#caf\xEF\xBF\xBD");
	assert_string_equal(rs.data[0].client, "");
	db_receipt_vec_free(&rs);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static void test_receipts_targets_webpush_certs(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user u;
	store_user(db, &u, "alice");
	struct db_network n;
	store_network(db, u.id, &n, "irc.example.org");

	struct db_delivery_receipt r;
	db_delivery_receipt_init(&r);
	xfree(r.target);
	r.target = xstrdup("#a");
	xfree(r.internal_msg_id);
	r.internal_msg_id = xstrdup("id1");
	T(db_store_client_delivery_receipts, db, n.id, "", &r, 1);
	T(db_store_client_delivery_receipts, db, n.id, "", &r, 1);
	T(db_store_client_delivery_receipts, db, n.id, "phone", &r, 1);
	db_receipt_vec rs = {0};
	T(db_list_delivery_receipts, db, n.id, &rs);
	assert_int_equal(rs.len, 2);
	db_receipt_vec_free(&rs);
	db_delivery_receipt_free(&r);

	struct db_read_receipt rr;
	bool found;
	T(db_get_read_receipt, db, n.id, "#a", &rr, &found);
	assert_false(found);
	db_read_receipt_init(&rr);
	xfree(rr.target);
	rr.target = xstrdup("#a");
	rr.timestamp = (struct db_time){.set = true, .t = timestamp_now()};
	T(db_store_read_receipt, db, n.id, &rr);
	T(db_store_read_receipt, db, n.id, &rr);
	struct db_read_receipt got;
	T(db_get_read_receipt, db, n.id, "#a", &got, &found);
	assert_true(found);
	assert_int_equal(got.id, rr.id);
	db_read_receipt_free(&got);
	db_read_receipt_free(&rr);

	struct db_message_target mt;
	T(db_get_message_target, db, n.id, "#a", &mt);
	assert_int_equal(mt.id, 0);
	mt.pinned = true;
	T(db_store_message_target, db, n.id, &mt);
	db_message_target_vec mts = {0};
	T(db_list_message_targets, db, n.id, &mts);
	assert_int_equal(mts.len, 1);
	assert_int_equal(mts.data[0].id, mt.id);
	assert_true(mts.data[0].pinned);
	db_message_target_vec_free(&mts);
	mt.pinned = false;
	T(db_store_message_target, db, n.id, &mt);
	T(db_list_message_targets, db, n.id, &mts);
	assert_int_equal(mts.len, 0);
	db_message_target_free(&mt);

	struct db_webpush_config cfg;
	db_webpush_config_init(&cfg);
	xfree(cfg.vapid_public);
	cfg.vapid_public = xstrdup("pub");
	xfree(cfg.vapid_private);
	cfg.vapid_private = xstrdup("priv");
	T(db_store_webpush_config, db, &cfg);
	db_webpush_config_vec cfgs = {0};
	T(db_list_webpush_configs, db, &cfgs);
	assert_int_equal(cfgs.len, 1);
	assert_int_equal(cfgs.data[0].id, cfg.id);
	assert_string_equal(cfgs.data[0].vapid_private, "priv");
	db_webpush_config_vec_free(&cfgs);
	db_webpush_config_free(&cfg);
	struct db_webpush_subscription sub;
	db_webpush_subscription_init(&sub);
	xfree(sub.endpoint);
	sub.endpoint = xstrdup("https://push.example.org/x");
	T(db_store_webpush_subscription, db, u.id, 0, &sub);
	db_webpush_sub_vec subs = {0};
	T(db_list_webpush_subscriptions, db, u.id, 0, &subs);
	assert_int_equal(subs.len, 1);
	db_webpush_sub_vec_free(&subs);
	i64vec nets = {0};
	T(db_list_webpush_networks, db, u.id, &nets);
	assert_int_equal(nets.len, 1);
	assert_int_equal(nets.data[0], 0);
	vec_free(&nets);
	T(db_list_webpush_subscriptions, db, u.id, n.id, &subs);
	assert_int_equal(subs.len, 0);
	T(db_delete_webpush_subscription, db, sub.id);
	T(db_list_webpush_subscriptions, db, u.id, 0, &subs);
	assert_int_equal(subs.len, 0);
	T(db_list_webpush_networks, db, u.id, &nets);
	assert_int_equal(nets.len, 0);
	db_webpush_subscription_free(&sub);

	char fp[64];
	memset(fp, 7, sizeof(fp));
	struct db_device_certificate cert;
	db_device_certificate_init(&cert);
	xfree(cert.label);
	cert.label = xstrdup("laptop");
	strbuf_append(&cert.fingerprint, fp, 64);
	db_device_certificate_mark_used(&cert, "127.0.0.1");
	T(db_store_device_certificate, db, u.id, &cert);
	struct db_device_certificate dup;
	db_device_certificate_init(&dup);
	strbuf_append(&dup.fingerprint, fp, 64);
	db_device_certificate_mark_used(&dup, nullptr);
	struct err *e = nullptr;
	assert_false(db_store_device_certificate(db, u.id, &dup, &e));
	assert_int_equal(e->code, DB_ERR_DUPLICATE_DEVICE_CERTIFICATE);
	err_free(e);
	db_device_certificate_free(&dup);
	int64_t owner;
	struct db_device_certificate gc;
	T(db_get_device_certificate, db, str_new(fp, 64), &owner, &gc, &found);
	assert_true(found);
	assert_int_equal(owner, u.id);
	assert_string_equal(gc.last_ip, "127.0.0.1");
	db_device_certificate_free(&gc);
	db_cert_vec certs = {0};
	T(db_list_device_certificates, db, u.id, &certs);
	assert_int_equal(certs.len, 1);
	db_cert_vec_free(&certs);
	T(db_delete_device_certificate, db, u.id, str_new(fp, 64));
	T(db_get_device_certificate, db, str_new(fp, 64), &owner, &gc, &found);
	assert_false(found);
	db_device_certificate_free(&cert);

	struct db_stats st;
	T(db_stats, db, &st);
	assert_int_equal(st.users, 1);
	assert_int_equal(st.networks, 1);
	assert_int_equal(st.channels, 0);
	db_network_free(&n);
	db_user_free(&u);
	db_close(db);
}

static void fts_eq(const char *in, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	quote_fts_query(&sb, str_from(in));
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void test_fts_quote(void **state) {
	(void)state;
	fts_eq("foo bar", "\"foo\" \"bar\"");
	fts_eq("a\"b OR c*", "\"a\" \"b\" \"OR\" \"c\"");
	fts_eq("", "");
}

// SQLite version 0 schema, from upstream. DO NOT EDIT.
static const char SQLITE_V0_SCHEMA[] = "\n"
				       "CREATE TABLE User (\n"
				       "\tusername VARCHAR(255) NOT NULL UNIQUE,\n"
				       "\tpassword VARCHAR(255)\n"
				       ");\n"
				       "\n"
				       "CREATE TABLE Network (\n"
				       "\tid INTEGER PRIMARY KEY,\n"
				       "\tname VARCHAR(255),\n"
				       "\tuser VARCHAR(255) NOT NULL,\n"
				       "\taddr VARCHAR(255) NOT NULL,\n"
				       "\tnick VARCHAR(255) NOT NULL,\n"
				       "\tusername VARCHAR(255),\n"
				       "\trealname VARCHAR(255),\n"
				       "\tpass VARCHAR(255),\n"
				       "\tsasl_mechanism VARCHAR(255),\n"
				       "\tsasl_plain_username VARCHAR(255),\n"
				       "\tsasl_plain_password VARCHAR(255),\n"
				       "\tUNIQUE(user, addr, nick),\n"
				       "\tUNIQUE(user, name)\n"
				       ");\n"
				       "\n"
				       "CREATE TABLE Channel (\n"
				       "\tid INTEGER PRIMARY KEY,\n"
				       "\tnetwork INTEGER NOT NULL,\n"
				       "\tname VARCHAR(255) NOT NULL,\n"
				       "\tkey VARCHAR(255),\n"
				       "\tFOREIGN KEY(network) REFERENCES Network(id),\n"
				       "\tUNIQUE(network, name)\n"
				       ");\n"
				       "\n"
				       "PRAGMA user_version = 1;\n";

static sqlite3 *raw_conn(void) {
	sqlite3 *c;
	assert_int_equal(sqlite3_open(":memory:", &c), SQLITE_OK);
	return c;
}

static void raw_exec(sqlite3 *c, const char *sql) {
	char *msg = nullptr;
	if (sqlite3_exec(c, sql, nullptr, nullptr, &msg) != SQLITE_OK) {
		fail_msg("%s", msg);
	}
}

static int64_t raw_i64(sqlite3 *c, const char *sql) {
	sqlite3_stmt *st;
	assert_int_equal(sqlite3_prepare_v2(c, sql, -1, &st, nullptr), SQLITE_OK);
	assert_int_equal(sqlite3_step(st), SQLITE_ROW);
	int64_t v = sqlite3_column_int64(st, 0);
	sqlite3_finalize(st);
	return v;
}

static void test_migrations(void **state) {
	(void)state;
	sqlite3 *c = raw_conn();
	raw_exec(c, SQLITE_V0_SCHEMA);
	raw_exec(c, "INSERT INTO User(username, password) VALUES ('alice', NULL);\n"
		    "        INSERT INTO Network(name, user, addr, nick) VALUES ('net', 'alice', "
		    "'irc.example.org', 'alice');\n"
		    "        INSERT INTO Channel(network, name) VALUES (1, '#soju');");
	struct db *db = nullptr;
	T(db_open_handle, c, &db);
	db_user_vec users = {0};
	T(db_list_users, db, &users);
	assert_int_equal(users.len, 1);
	db_network_vec nets = {0};
	T(db_list_networks, db, users.data[0].id, &nets);
	assert_string_equal(nets.data[0].name, "net");
	assert_true(nets.data[0].enabled && nets.data[0].auto_away);
	db_channel_vec chs = {0};
	T(db_list_channels, db, nets.data[0].id, &chs);
	assert_string_equal(chs.data[0].name, "#soju");
	assert_int_equal(raw_i64(db_handle(db), "PRAGMA user_version"), db_schema_version());
	assert_int_equal(db_schema_version(), 33);
	db_channel_vec_free(&chs);
	db_network_vec_free(&nets);
	db_user_vec_free(&users);
	db_close(db);
}

static void test_message_text_index_migration(void **state) {
	(void)state;
	for (int manual_index = 0; manual_index < 2; manual_index++) {
		// Build the previous schema using its original migrations
		sqlite3 *c = raw_conn();
		raw_exec(c, SQLITE_V0_SCHEMA);
		for (int i = 1; i < 30; i++) {
			raw_exec(c, db_migration((size_t)i));
		}
		raw_exec(c,
			 "PRAGMA user_version = 30;\n"
			 "            INSERT INTO User(id, username) VALUES (1, 'alice');\n"
			 "            INSERT INTO Network(id, user, addr) VALUES (1, 1, "
			 "'irc.example.org');\n"
			 "            INSERT INTO MessageTarget(id, network, target) VALUES (1, 1, "
			 "'#a');\n"
			 "            INSERT INTO Message(target, raw, time, sender, text) VALUES\n"
			 "                (1, 'PRIVMSG #a :hello', '2024-01-01T00:00:00.000Z', '', "
			 "'hello'),\n"
			 "                (1, 'JOIN #a', '2024-01-01T00:00:01.000Z', '', NULL);");
		if (manual_index) {
			raw_exec(c, "CREATE INDEX MessageTextIndex ON Message(target, time) WHERE "
				    "text IS NOT NULL;");
		}
		struct db *db = nullptr;
		T(db_open_handle, c, &db);
		struct db_message_options o = {.limit = 10, .take_last = true};
		assert_msgs(db, 1, "#a", &o, (const char *[]){"PRIVMSG #a hello"}, 1);
		db_target_last_vec targets = {0};
		T(db_list_message_last_per_target, db, 1, &o, &targets);
		struct db_time want = at("2024-01-01T00:00:00.000Z");
		assert_int_equal(timestamp_cmp(targets.data[0].latest_message.t, want.t), 0);
		db_target_last_vec_free(&targets);
		o.events = true;
		assert_int_equal(count_msgs(db, 1, "#a", &o), 2);
		assert_int_equal(raw_i64(db_handle(db), "PRAGMA user_version"),
				 db_schema_version());
		sqlite3_stmt *st;
		assert_int_equal(sqlite3_prepare_v2(db_handle(db),
						    "SELECT stat FROM sqlite_stat1 WHERE idx = "
						    "'MessageTextIndex'",
						    -1, &st, nullptr),
				 SQLITE_OK);
		assert_int_equal(sqlite3_step(st), SQLITE_ROW);
		// Only the text message is indexed, and the migration analyzed it
		assert_string_equal((const char *)sqlite3_column_text(st, 0), "1 1 1");
		sqlite3_finalize(st);
		// Opening an already upgraded database is harmless
		T(db_upgrade, db);
		db_close(db);
	}
}

static void test_message_text_index_query_plans(void **state) {
	(void)state;
	struct db *db = open_db();
	sqlite3 *c = db_handle(db);
	raw_exec(c,
		 "INSERT INTO User(id, username, created_at) VALUES (1, 'alice', "
		 "'2024-01-01T00:00:00.000Z');\n"
		 "        INSERT INTO Network(id, user, addr) VALUES (1, 1, 'irc.example.org');\n"
		 "        WITH RECURSIVE targets(id) AS (\n"
		 "            VALUES (1) UNION ALL SELECT id + 1 FROM targets WHERE id < 16\n"
		 "        )\n"
		 "        INSERT INTO MessageTarget(id, network, target)\n"
		 "            SELECT id, 1, '#channel' || id FROM targets;\n"
		 "        WITH RECURSIVE messages(id) AS (\n"
		 "            VALUES (1) UNION ALL SELECT id + 1 FROM messages WHERE id < 100\n"
		 "        )\n"
		 "        INSERT INTO Message(target, raw, time, sender, text)\n"
		 "            SELECT t.id, 'raw', printf('2024-01-01T00:%02d:%02d.000Z', m.id / "
		 "60, m.id % 60),\n"
		 "                '', CASE WHEN m.id % 10 = 0 THEN 'hello' END\n"
		 "            FROM MessageTarget t, messages m;");
	// Check both a fresh database without statistics and one with them
	for (int analyze = 0; analyze < 2; analyze++) {
		if (analyze) {
			raw_exec(c, "ANALYZE;");
		}
		for (int events = 0; events < 2; events++) {
			const char *filter = events ? "text IS NULL" : "text IS NOT NULL";
			const char *index = events ? "MessageEventIndex" : "MessageTextIndex";
			char queries[2][512];
			snprintf(
				queries[0], sizeof(queries[0]),
				"SELECT target, (SELECT time FROM Message WHERE "
				"target=MessageTarget.id AND %s ORDER BY time DESC LIMIT 1) latest "
				"FROM MessageTarget WHERE network=1 ORDER BY latest DESC LIMIT 10",
				filter);
			snprintf(queries[1], sizeof(queries[1]),
				 "SELECT m.raw FROM Message m,MessageTarget t WHERE m.target=t.id "
				 "AND "
				 "t.network=1 AND t.target='#channel1' AND m.%s ORDER BY m.time "
				 "DESC,m.id DESC LIMIT 5",
				 filter);
			for (int q = 0; q < 2; q++) {
				AUTOFREE char *sql = xasprintf("EXPLAIN QUERY PLAN %s", queries[q]);
				sqlite3_stmt *st;
				assert_int_equal(sqlite3_prepare_v2(c, sql, -1, &st, nullptr),
						 SQLITE_OK);
				bool used = false;
				while (sqlite3_step(st) == SQLITE_ROW) {
					const char *detail =
						(const char *)sqlite3_column_text(st, 3);
					used = used || strstr(detail, index) != nullptr;
				}
				sqlite3_finalize(st);
				if (!used) {
					fail_msg("expected %s (analyze=%d)", index, analyze);
				}
			}
		}
	}
	db_close(db);
}

static void test_newer_schema_rejected(void **state) {
	(void)state;
	sqlite3 *c = raw_conn();
	raw_exec(c, "PRAGMA user_version = 999");
	struct db *db = nullptr;
	struct err *e = nullptr;
	assert_false(db_open_handle(c, &db, &e));
	assert_non_null(strstr(err_msg(e), "older than schema"));
	err_free(e);
}

static int64_t total_changes(struct db *db) {
	return raw_i64(db_handle(db), "SELECT total_changes()");
}

static void test_unchanged_records_and_receipts_do_not_write(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user user;
	store_user(db, &user, "cost-test");
	struct db_network network;
	store_network(db, user.id, &network, "irc.cost-test");
	struct db_channel channel;
	db_channel_init(&channel);
	xfree(channel.name);
	channel.name = xstrdup("#cost");
	T(db_store_channel, db, network.id, &channel);
	struct db_message_target target;
	T(db_get_message_target, db, network.id, "#cost", &target);
	T(db_store_message_target, db, network.id, &target);
	struct db_read_receipt receipt;
	db_read_receipt_init(&receipt);
	xfree(receipt.target);
	receipt.target = xstrdup("#cost");
	receipt.timestamp = (struct db_time){.set = true, .t = timestamp_now()};
	T(db_store_read_receipt, db, network.id, &receipt);
	struct db_device_certificate cert;
	db_device_certificate_init(&cert);
	xfree(cert.label);
	cert.label = xstrdup("cost");
	char fp[64];
	memset(fp, 17, sizeof(fp));
	strbuf_append(&cert.fingerprint, fp, 64);
	cert.last_used = (struct db_time){.set = true, .t = timestamp_now()};
	T(db_store_device_certificate, db, user.id, &cert);
	struct db_delivery_receipt delivered[2];
	db_delivery_receipt_init(&delivered[0]);
	xfree(delivered[0].target);
	delivered[0].target = xstrdup("#cost");
	xfree(delivered[0].internal_msg_id);
	delivered[0].internal_msg_id = xstrdup("same");
	T(db_store_client_delivery_receipts, db, network.id, "", delivered, 1);
	int64_t cursor = delivered[0].id;
	int64_t before = total_changes(db);
	T(db_store_user, db, &user);
	T(db_store_user_keep_password, db, &user);
	T(db_store_network, db, user.id, &network);
	T(db_store_channel, db, network.id, &channel);
	T(db_store_message_target, db, network.id, &target);
	T(db_store_read_receipt, db, network.id, &receipt);
	T(db_store_device_certificate, db, user.id, &cert);
	T(db_store_client_delivery_receipts, db, network.id, "", delivered, 1);
	assert_int_equal(total_changes(db), before);
	assert_int_equal(delivered[0].id, cursor);
	xfree(delivered[0].internal_msg_id);
	delivered[0].internal_msg_id = xstrdup("changed");
	db_delivery_receipt_init(&delivered[1]);
	xfree(delivered[1].target);
	delivered[1].target = xstrdup("#new");
	xfree(delivered[1].internal_msg_id);
	delivered[1].internal_msg_id = xstrdup("new");
	T(db_store_client_delivery_receipts, db, network.id, "", delivered, 2);
	assert_int_equal(delivered[0].id, cursor);
	assert_int_not_equal(delivered[1].id, 0);
	T(db_store_client_delivery_receipts, db, network.id, "", &delivered[1], 1);
	db_receipt_vec rs = {0};
	T(db_list_delivery_receipts, db, network.id, &rs);
	assert_int_equal(rs.len, 1);
	db_receipt_vec_free(&rs);
	db_delivery_receipt_free(&delivered[0]);
	db_delivery_receipt_free(&delivered[1]);
	db_device_certificate_free(&cert);
	db_read_receipt_free(&receipt);
	db_message_target_free(&target);
	db_channel_free(&channel);
	db_network_free(&network);
	db_user_free(&user);
	db_close(db);
}

static void test_sparse_fts_preserves_updates_and_event_history_ordering(void **state) {
	(void)state;
	struct db *db = open_db();
	struct db_user user;
	store_user(db, &user, "fts-test");
	struct db_network network;
	store_network(db, user.id, &network, "irc.fts-test");
	const char *t = "2024-01-01T00:00:00.000Z";
	struct irc_msg *m[3] = {msg_at("PRIVMSG #cost hello", t), msg_at("JOIN #cost", t),
				msg_at("NOTICE #cost world", t)};
	char *s[3];
	for (int i = 0; i < 3; i++) {
		s[i] = irc_msg_to_string(m[i]);
	}
	i64vec ids = {0};
	store_msgs(db, network.id, "#cost", m, 3, &ids);
	for (int events = 1; events >= 0; events--) {
		const char *wanted_all[] = {s[0], s[1], s[2]};
		const char *wanted_text[] = {s[0], s[2]};
		const char *const *wanted = events ? wanted_all : wanted_text;
		size_t nw = events ? 3 : 2;
		for (int take_last = 0; take_last < 2; take_last++) {
			struct db_message_options o = {
				.limit = 2, .events = events, .take_last = take_last};
			assert_msgs(db, network.id, "#cost", &o,
				    take_last ? wanted + (nw - 2) : wanted, 2);
		}
	}
	sqlite3 *c = db_handle(db);
	char sql[128];
	snprintf(sql, sizeof(sql), "UPDATE Message SET text=NULL WHERE id=%lld",
		 (long long)ids.data[0]);
	raw_exec(c, sql);
	snprintf(sql, sizeof(sql), "UPDATE Message SET text='fresh' WHERE id=%lld",
		 (long long)ids.data[1]);
	raw_exec(c, sql);
	snprintf(sql, sizeof(sql), "UPDATE Message SET id=999 WHERE id=%lld",
		 (long long)ids.data[1]);
	raw_exec(c, sql);
	assert_int_equal(raw_i64(c, "SELECT rowid FROM MessageFTS WHERE MessageFTS MATCH 'fresh'"),
			 999);
	assert_int_equal(
		raw_i64(c, "SELECT count(*) FROM MessageFTS WHERE MessageFTS MATCH 'hello'"), 0);
	raw_exec(c, "INSERT INTO MessageFTS(MessageFTS,rank)VALUES('integrity-check',1)");
	for (int i = 0; i < 3; i++) {
		irc_msg_free(m[i]);
		xfree(s[i]);
	}
	vec_free(&ids);
	db_network_free(&network);
	db_user_free(&user);
	db_close(db);
}

// src/database/mod.rs
static void test_password(void **state) {
	(void)state;
	struct db_user u;
	db_user_init(&u, "a");
	T(db_user_set_password, &u, "secret");
	assert_true(strncmp(u.password, "$2a$10$", 7) == 0);
	bool upgraded;
	T(db_user_check_password, &u, "secret", &upgraded);
	assert_false(upgraded);
	struct err *e = nullptr;
	assert_false(db_user_check_password(&u, "wrong", &upgraded, &e));
	assert_string_equal(err_msg(e), "wrong password: crypto/bcrypt: hashedPassword is not the "
					"hash of the given password");
	err_free(e);
	char pw[74];
	memset(pw, 'x', 73);
	pw[72] = '\0';
	T(db_user_set_password, &u, pw);
	pw[72] = 'x';
	pw[73] = '\0';
	e = nullptr;
	assert_false(db_user_set_password(&u, pw, &e));
	err_free(e);
	db_user_free(&u);

	// A hash made with cost 4 (bcrypt::hash's $2b$ format) gets upgraded
	db_user_init(&u, "a");
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
	assert_non_null(crypt_gensalt_rn("$2b$", 4, nullptr, 0, salt, sizeof(salt)));
	struct crypt_data *data = xcalloc(1, sizeof(*data));
	xfree(u.password);
	u.password = xstrdup(crypt_rn("pw", salt, data, (int)sizeof(*data)));
	xfree(data);
	T(db_user_check_password, &u, "pw", &upgraded);
	assert_true(upgraded);
	assert_true(strncmp(u.password, "$2a$10$", 7) == 0);
	// C-only: errors of disabled and malformed hashes
	xfree(u.password);
	u.password = xstrdup("");
	e = nullptr;
	assert_false(db_user_check_password(&u, "pw", &upgraded, &e));
	assert_string_equal(err_msg(e), "password auth disabled");
	err_free(e);
	xfree(u.password);
	u.password = xstrdup("$2a$10$short");
	e = nullptr;
	assert_false(db_user_check_password(&u, "pw", &upgraded, &e));
	assert_string_equal(err_msg(e), "wrong password: Invalid hash: the hash format is "
					"malformed; expected 60 ASCII bytes");
	err_free(e);
	db_user_free(&u);
}

T_MAIN(cmocka_unit_test(test_users), cmocka_unit_test(test_networks_channels),
       cmocka_unit_test(test_messages), cmocka_unit_test(test_messages_same_time),
       cmocka_unit_test(test_message_ids_not_reused),
       cmocka_unit_test(test_delete_messages_in_chunks),
       cmocka_unit_test(test_message_last_id_insertion_order),
       cmocka_unit_test(test_delete_messages_vacuum), cmocka_unit_test(test_messages_invalid_utf8),
       cmocka_unit_test(test_receipts_targets_webpush_certs), cmocka_unit_test(test_fts_quote),
       cmocka_unit_test(test_migrations), cmocka_unit_test(test_message_text_index_migration),
       cmocka_unit_test(test_message_text_index_query_plans),
       cmocka_unit_test(test_newer_schema_rejected),
       cmocka_unit_test(test_unchanged_records_and_receipts_do_not_write),
       cmocka_unit_test(test_sparse_fts_preserves_updates_and_event_history_ordering),
       cmocka_unit_test(test_password))
