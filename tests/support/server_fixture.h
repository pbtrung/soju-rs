#pragma once
#include "testsupport.h"
#include "user/user.h"

struct test_peer {
	int fd;
	struct strbuf pending;
	bool reply_ping;
};
struct server_fixture {
	struct loop_pool loops;
	struct db_thread *db;
	struct server srv;
	struct db_user user;
	struct db_network network;
	uv_thread_t runner;
	int listener;
	unsigned next_conn;
	VEC(struct test_peer *) peers;
};
// Takes ownership of db through the fixture DB thread.
void fixture_init_database(struct server_fixture *e, struct db *db, bool memory);
// Like fixture_init_database, calling before_start(e, data) once the user and
// network are stored, before the server starts
void fixture_init_database_hook(struct server_fixture *e, struct db *db, bool memory,
				void (*before_start)(struct server_fixture *e, void *data),
				void *data);
void fixture_init(struct server_fixture *e, bool memory);
void fixture_finish(struct server_fixture *e);
[[nodiscard]] struct test_peer *fixture_upstream(struct server_fixture *e, const char *caps,
						 const char *isupport);
[[nodiscard]] struct test_peer *fixture_connect(struct server_fixture *e);
// An HTTP connection to the server (from 127.0.0.1): the client's socket
[[nodiscard]] int fixture_connect_http(struct server_fixture *e, bool prometheus);
void fixture_register(struct server_fixture *e, struct test_peer *p, bool network,
		      const char *caps);
void peer_send(struct test_peer *p, const char *line);
void peer_send_take(struct test_peer *p, struct irc_msg *msg);
[[nodiscard]] struct irc_msg *peer_read(struct test_peer *p);
[[nodiscard]] struct irc_msg *peer_until(struct test_peer *p, const char *command);
void peer_sync(struct test_peer *p);
// Returns messages seen before the matching PONG; batches/CAP are omitted.
void peer_roundtrip(struct test_peer *p, irc_msg_vec *out);
void fixture_run_user(struct server_fixture *e, user_run_fn fn, void *data);
