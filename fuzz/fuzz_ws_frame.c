// WebSocket frame reading (PLAN.md 2.3 L5, fuzz_ws_frame): no crash, the
// limits hold, and reading the input in two pieces gives the same events
// as reading it at once. The first input byte picks the split point and
// the role (server or client).
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "http/websocket.h"
#include "net/conn.h"

// The events of input, read in pieces split at split; appended to log as
// "<kind>:<payload>" records
static void read_all(bool server, struct str in, size_t split, struct strbuf *log) {
	struct ws_reader r;
	ws_reader_init(&r, server);
	struct strbuf buf = STRBUF_INIT;
	size_t fed = 0;
	for (;;) {
		size_t used;
		struct ws_event ev;
		enum ws_event_kind k = ws_read(&r, strbuf_str(&buf), &used, &ev);
		if (used > buf.len) {
			abort();
		}
		strbuf_drop_front(&buf, used);
		if (k == WS_MESSAGE && ev.payload.len > MAX_LINE_LENGTH) {
			abort();
		}
		if (k == WS_NEED_MORE) {
			ws_event_free(&ev);
			if (fed == in.len) {
				break;
			}
			size_t end = fed < split ? split : in.len;
			strbuf_append(&buf, in.p + fed, end - fed);
			fed = end;
			continue;
		}
		strbuf_printf(log, "%d:%d:", (int)k, ev.close_code);
		strbuf_append(log, ev.payload.buf != nullptr ? ev.payload.buf : "", ev.payload.len);
		strbuf_append_char(log, '\n');
		ws_event_free(&ev);
		if (k == WS_ERROR || k == WS_CLOSE) {
			break;
		}
	}
	strbuf_free(&buf);
	ws_reader_finish(&r);
}

// NOLINTNEXTLINE(misc-use-internal-linkage)
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	if (size < 1) {
		return 0;
	}
	bool server = (data[0] & 1U) == 0;
	struct str in = str_new((const char *)data + 1, size - 1);
	size_t split = in.len > 0 ? (data[0] >> 1U) % (in.len + 1) : 0;
	struct strbuf whole = STRBUF_INIT, pieces = STRBUF_INIT;
	read_all(server, in, in.len, &whole);
	read_all(server, in, split, &pieces);
	if (!str_eq(strbuf_str(&whole), strbuf_str(&pieces))) {
		abort();
	}
	strbuf_free(&whole);
	strbuf_free(&pieces);
	return 0;
}
