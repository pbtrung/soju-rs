// IRC connections over libuv streams (TCP, Unix sockets), optionally TLS,
// optionally behind a PROXY header (soju-rs conn.rs, plus the TLS and PROXY
// steps of listen.rs).
//
// A connection belongs to one loop. Before IRC lines flow it may go through
// a TLS handshake and a PROXY header; then the "ready" callback runs, and
// "message" for each line read. Writes are queued, batched, rate limited if
// asked, and time out after WRITE_TIMEOUT.
//
// Lifetime: the creator holds a reference (conn_unref when done). The
// connection stays allocated until its handles are closed and every
// reference is gone.
#pragma once

#include <openssl/ssl.h>
#include <stdint.h>
#include <uv.h>

#include "base/err.h"
#include "base/log.h"
#include "base/ref.h"
#include "base/str.h"
#include "base/vec.h"
#include "irc/message.h"
#include "net/proxyproto.h"
#include "net/rate.h"
#include "net/tls.h"

static constexpr uint64_t CONN_WRITE_TIMEOUT_MS = 10000;
// Maximum length of an incoming line or WebSocket message. IRCv3 lines are
// at most 8191 bytes of tags plus 512 bytes: leave room for servers with a
// larger LINELEN.
static constexpr size_t MAX_LINE_LENGTH = 32768;
// Maximum size of the messages queued on a connection and not written yet;
// a peer which doesn't read fast enough is disconnected once it's reached
static constexpr size_t MAX_QUEUED_BYTES = (size_t)16 << 20U;
// Accounted size of a queued line on top of its length
static constexpr size_t QUEUED_LINE_OVERHEAD = 64;
// Maximum number of messages read and still being processed: once reached,
// the connection isn't read anymore (TCP flow control slows the peer down)
static constexpr size_t MAX_INPUT_IN_FLIGHT = 64;

struct conn;

struct conn_callbacks {
	// The TLS handshake and PROXY header are done; lines will follow. May be
	// nullptr.
	void (*ready)(struct conn *c, void *data);
	// A message was read (owned by the callee). The callee calls
	// conn_input_done once it's processed.
	void (*message)(struct conn *c, struct irc_msg *msg, void *data);
	// The connection is closed (by the peer, an error, or us); err is
	// nullptr for EOF or a local close. Called once.
	void (*closed)(struct conn *c, const struct err *err, void *data);
};

// Rate limiting of outgoing messages: one message every delay_ns with a
// burst, while *enabled (upstreams turn it off on soju.im/SAFERATE)
struct conn_rate {
	uint64_t delay_ns;
	uint32_t burst;
	const bool *enabled;
};

struct conn_options {
	SSL_CTX *tls_server; // TLS server handshake first, or nullptr
	SSL_CTX *tls_client; // TLS client handshake first, or nullptr
	const char *tls_host; // server name for tls_client
	bool proxy; // read an optional PROXY header (after TLS)
	// Timeout of the TLS and PROXY steps (0: none)
	uint64_t setup_timeout_ms;
	const struct conn_rate *rate; // nullptr: no rate limiting
};

// Takes ownership of stream (an initialized uv_tcp_t or uv_pipe_t on the
// current loop, allocated with xmalloc). log is copied.
[[nodiscard]] struct conn *conn_new(uv_stream_t *stream, const struct conn_options *opts,
				    const struct logger *log, const struct conn_callbacks *cb,
				    void *data);
void conn_set_callbacks(struct conn *c, const struct conn_callbacks *cb, void *data);
void conn_ref(struct conn *c);
void conn_unref(struct conn *c);

// After ready: what the setup learned (borrowed, valid while referenced)
[[nodiscard]] const struct proxy_header *conn_proxy_header(const struct conn *c);
// The DER client certificate of the TLS handshake, or nullptr
[[nodiscard]] const struct strbuf *conn_peer_cert(const struct conn *c);
[[nodiscard]] struct str conn_alpn(const struct conn *c);
[[nodiscard]] uv_stream_t *conn_stream(const struct conn *c);

// Queues a message (borrowed). Dropped once closed; closes the connection
// if more than MAX_QUEUED_BYTES would be queued.
void conn_send(struct conn *c, const struct irc_msg *msg);
// Closes the connection immediately. Safe to call several times.
void conn_close(struct conn *c);
// Closes gracefully, after writing the queued messages
void conn_shutdown(struct conn *c);
[[nodiscard]] bool conn_is_closed(const struct conn *c);
// The number of messages queued and not written yet
[[nodiscard]] size_t conn_queued(const struct conn *c);
// A message delivered by the message callback has been processed
void conn_input_done(struct conn *c);

// A virtual connection on c (soju.im/multiplex): its messages carry the
// tag key=value, and closing it calls on_close instead of closing c
struct conn_view;

[[nodiscard]] struct conn_view *conn_view_new(struct conn *c, const char *tag_key,
					      const char *tag_value, void (*on_close)(void *data),
					      void *data);
void conn_view_free(struct conn_view *v);
void conn_view_send(struct conn_view *v, const struct irc_msg *msg);
// Unless closed quietly, the peer gets an ERROR with the tag first
void conn_view_close(struct conn_view *v);
void conn_view_close_quietly(struct conn_view *v);
[[nodiscard]] bool conn_view_is_closed(const struct conn_view *v);

// Formats a message as a line without CRLF; line breaks can't inject
// another message (exposed for tests)
void conn_format_line(const struct irc_msg *msg, struct strbuf *out);
// Parses a received line: only the part before the first CR or LF is
// used, decoded lossily; *msg is nullptr for an empty line
[[nodiscard]] bool conn_parse_line(struct str line, struct irc_msg **msg, struct err **err);
// Rust's std::io::Error Display for a libuv error ("Connection reset by
// peer (os error 104)")
void io_error_str(struct strbuf *out, int uv_err);
