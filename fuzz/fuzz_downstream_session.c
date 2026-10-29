// Stateful downstream registration / CAP / SASL / multiplex lifecycle.
// Newlines delimit input. SOH closes the connection, STX creates a virtual
// network after registration, ETX detaches it. Every input tears down all
// handles and checks the live-object table.
#include <assert.h>
#include <stdint.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "downstream/downstream.h"

struct session {
	struct server srv;
	struct loop_pool loops;
	struct db_thread *db;
	uint64_t dc_id;
};

static void registered(struct downstream *dc, void *data) {
	(void)data;
	struct downstream_sync s = {.nick = "fuzzer",
				    .username = "fuzzer",
				    .hostname = "localhost",
				    .realname = "Fuzzer",
				    .account = "fuzzer",
				    .casemap = CASEMAPPING_ASCII};
	downstream_welcome(dc, &s);
}

static void message(struct downstream *dc, struct irc_msg *msg, void *data) {
	(void)data;
	downstream_begin_command(dc, msg);
	if (cstr_eq(msg->command, "PING")) {
		downstream_send(dc, IRC_MSG("PONG", irc_msg_trailing(msg)));
	}
	downstream_end_command(dc);
	irc_msg_free(msg);
	downstream_input_done(dc);
}

// NOLINTNEXTLINE(misc-use-internal-linkage)
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	if (size > 65536) {
		return 0;
	}
	struct session s = {0};
	loop_pool_init(&s.loops, 1);
	loop_set_current(&s.loops.loops[0]);
	struct db *db = nullptr;
	assert(db_open_in_memory(&db, nullptr));
	assert(db_thread_start(&s.loops.loops[0].uv, db, 0, &s.db, nullptr));
	server_init(&s.srv, &s.loops, s.db);
	int fds[2];
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	uv_pipe_t *pipe = xcalloc(1, sizeof(*pipe));
	assert(uv_pipe_init(&s.loops.loops[0].uv, pipe, 0) == 0);
	assert(uv_pipe_open(pipe, fds[0]) == 0);
	struct conn *c = conn_new((uv_stream_t *)pipe, &(struct conn_options){0}, &s.srv.log,
				  nullptr, nullptr);
	s.dc_id = atomic_load(&s.srv.next_downstream_id);
	downstream_serve(&s.srv, c, &(struct conn_info){.remote_addr = (char *)(uintptr_t)"local"},
			 &(struct downstream_events){.registered = registered, .message = message});
	struct str rest = str_new((const char *)data, size), line;
	while (str_next_field(&rest, '\n', &line)) {
		struct downstream *dc = downstream_lookup(s.dc_id);
		if (dc == nullptr) {
			break;
		}
		if (line.len > 0 && line.p[0] == '\1') {
			downstream_close(dc);
		} else if (line.len > 0 && line.p[0] == '\2') {
			// A test authentication completion, allowing the same corpus to
			// reach registered commands without spending CPU on bcrypt.
			dc->registered = true;
			uv_timer_stop(&dc->register_timer);
			registered(dc, nullptr);
			cap_registry_set_enabled(&dc->caps, "soju.im/multiplex", true);
			struct downstream *child = downstream_attach(dc, 1);
			registered(child, nullptr);
		} else if (line.len > 0 && line.p[0] == '\3') {
			struct downstream *child = downstream_find_attached(dc->id, 1);
			if (child != nullptr) {
				downstream_detach(child);
			}
		} else {
			// Actual connection input exercises parsing and asynchronous
			// registration, with the normal 64-message flow control.
			(void)send(fds[1], line.p, line.len, MSG_NOSIGNAL | MSG_DONTWAIT);
			(void)send(fds[1], "\n", 1, MSG_NOSIGNAL | MSG_DONTWAIT);
		}
		uv_run(&s.loops.loops[0].uv, UV_RUN_NOWAIT);
		char buf[4096];
		while (recv(fds[1], buf, sizeof(buf), MSG_DONTWAIT) > 0) {
		}
	}
	struct downstream *dc = downstream_lookup(s.dc_id);
	if (dc != nullptr) {
		downstream_close(dc);
	}
	close(fds[1]);
	db_thread_stop(s.db);
	server_finish(&s.srv);
	loop_close_inbox(&s.loops.loops[0]);
	uv_run(&s.loops.loops[0].uv, UV_RUN_DEFAULT);
	loop_set_current(nullptr);
	loop_pool_free(&s.loops);
	assert(ref_live_count(nullptr) == 0);
	return 0;
}
