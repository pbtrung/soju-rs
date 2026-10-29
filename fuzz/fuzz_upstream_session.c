// Hostile upstream sessions, interleaved with command and timer events.
#include <assert.h>
#include <stdint.h>

#include "base/ref.h"
#include "upstream/upstream.h"

static void discard_send(struct upstream *uc, struct irc_msg *msg, void *data) {
	(void)uc;
	(void)data;
	irc_msg_free(msg);
}

static void discard_received(struct upstream *uc, struct irc_msg *msg, uint64_t id, void *data) {
	(void)id;
	discard_send(uc, msg, data);
}

// NOLINTNEXTLINE(misc-use-internal-linkage)
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	if (size > 65536) {
		return 0;
	}
	struct loop_pool loops;
	loop_pool_init(&loops, 1);
	loop_set_current(&loops.loops[0]);
	struct server srv;
	server_init(&srv, &loops, nullptr);
	struct db_user u;
	struct db_network n;
	db_user_init(&u, "fuzzer");
	db_network_init(&n, "irc+insecure://localhost:1");
	struct upstream *uc = upstream_new(
		&srv, &u, &n,
		&(struct upstream_events){.send = discard_send, .message = discard_received});
	db_user_free(&u);
	db_network_free(&n);
	upstream_register(uc);
	struct str rest = str_new((const char *)data, size), line;
	while (str_next_field(&rest, '\n', &line)) {
		if (line.len > 0 && line.p[0] == '\1') {
			break;
		}
		if (line.len > 0 && line.p[0] == '\2') {
			upstream_tick(uc, uc->now + 60000);
		} else if (line.len > 0 && line.p[0] == '\3') {
			bool queued = upstream_enqueue_take(uc, 1, "fuzz", IRC_MSG("WHOIS", "x"));
			(void)queued;
		} else {
			struct irc_msg *msg = nullptr;
			if (conn_parse_line(line, &msg, nullptr) && msg != nullptr) {
				upstream_receive_take(uc, msg);
			}
		}
	}
	upstream_free(uc);
	server_finish(&srv);
	loop_close_inbox(&loops.loops[0]);
	uv_run(&loops.loops[0].uv, UV_RUN_DEFAULT);
	loop_set_current(nullptr);
	loop_pool_free(&loops);
	assert(ref_live_count(nullptr) == 0);
	return 0;
}
