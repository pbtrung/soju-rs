// Internals of the connection files (net/conn.c, conn_read.c, conn_write.c)
// shared with net/conn_framing.c (raw and WebSocket framing)
#pragma once

#include "http/websocket.h"
#include "net/conn.h"

enum stage {
	STAGE_START, // no setup step: opens on the next loop iteration
	STAGE_TLS, // TLS handshake
	STAGE_PROXY, // PROXY header
	STAGE_OPEN, // IRC lines
};

struct conn {
	struct ref_node ref;
	struct conn *next, *prev; // registry on the owning thread
	uv_stream_t *stream;
	uv_timer_t write_timer, rate_timer, setup_timer;
	struct logger log;
	struct conn_callbacks cb;
	void *cb_data;
	struct tls_session tls;
	struct strbuf peer_cert;
	struct proxy_header proxy_header;
	struct strbuf pre; // bytes of the PROXY step
	struct strbuf lines; // plaintext not yet split into lines or frames
	size_t in_flight;
	enum conn_framing framing;
	struct ws_reader ws;

	// Outgoing
	struct strbuf raw_out; // bytes queued by conn_send_raw and control frames
	strvec queue; // lines without CRLF, oldest first
	size_t queue_head; // index of the first unsent line
	size_t queued_bytes;
	struct strbuf write_buf;
	uv_write_t write_req;
	uv_shutdown_t shutdown_req;
	// rate limiting
	const bool *rate_flag;
	struct rate_limiter limiter;
	char *rate_line; // dequeued, waiting for the limiter

	unsigned refs;
	unsigned handles_open; // handles not yet through their close callback
	enum stage stage;
	bool proxy;
	bool has_tls;
	bool has_peer_cert;
	bool eof;
	bool closed; // no more reading nor callbacks
	bool closed_notified;
	bool handles_closing;
	bool reading;
	bool writing;
	bool shutdown_requested;
	bool shutting_down;
	bool rate_enabled;
	bool rate_waiting;
	bool ws_close_sent;
	bool ws_peer_closed; // a close frame was read
};

// Stops everything; err is reported to the closed callback
void conn_fail(struct conn *c, const struct err *err);
[[gnu::format(printf, 2, 3)]] void conn_fail_msg(struct conn *c, const char *fmt, ...);
// Hands a parsed message to the message callback (counted in flight)
void conn_deliver(struct conn *c, struct irc_msg *msg);
void conn_flush_writes(struct conn *c);

// conn_framing.c: the input of the raw and WebSocket framings
void conn_process_raw(struct conn *c);
void conn_process_websocket(struct conn *c);
// Appends the plaintext for an outgoing line to out
void conn_frame_line(struct conn *c, const char *line, struct strbuf *out);
// Appends a WebSocket close frame (once) to out; false if not WebSocket or
// already sent
[[nodiscard]] bool conn_frame_close(struct conn *c, struct strbuf *out);

// Shared by net/conn.c, net/conn_read.c and net/conn_write.c
void conn_become_open(struct conn *c);
void conn_close_handles(struct conn *c);
void conn_flush_tls_output(struct conn *c);
void conn_notify_closed(struct conn *c, const struct err *err);
void conn_plaintext(struct conn *c, const char *p, size_t n);
void conn_process(struct conn *c);
void conn_update_reading(struct conn *c);
void conn_write_raw(struct conn *c);
