// Listeners (soju-rs listen.rs): TCP and Unix sockets for IRC, admin,
// ident, HTTP and Prometheus. Accepted sockets are handed to the loops
// round-robin (PLAN.md 2.1 rule 1).
#pragma once

#include <openssl/ssl.h>

#include "base/err.h"
#include "base/log.h"
#include "net/conn.h"
#include "net/identd.h"
#include "net/ipaddr.h"
#include "net/listen_uri.h"
#include "net/loops.h"

// TCP keep-alive interval for downstream TCP connections
static constexpr unsigned DOWNSTREAM_KEEP_ALIVE_SECS = 3600;
// Timeout of the TLS handshake and PROXY header
static constexpr uint64_t DOWNSTREAM_REGISTER_TIMEOUT_MS = 30000;

// About an accepted connection, once ready
struct conn_info {
	// "ip:port" (IPv4-mapped addresses as IPv4), "@" for Unix sockets
	char *remote_addr;
	bool has_remote_ip;
	struct ipaddr remote_ip; // canonical
	const struct strbuf *peer_cert; // DER, or nullptr; borrowed from the conn
};

struct listener_handlers {
	// Called on the connection's loop; the callee takes the reference on
	// c. info is valid during the call.
	void (*irc)(struct conn *c, const struct conn_info *info, void *data);
	void (*admin)(struct conn *c, const struct conn_info *info, void *data);
	// HTTP connections, after the TLS handshake (raw framing; the callee
	// takes the reference on c). nullptr: such connections are closed.
	void (*http)(struct conn *c, const struct conn_info *info, bool prometheus, void *data);
	struct identd *identd;
	// Whether PROXY headers from a peer are trusted (remote_ip nullptr for
	// Unix sockets). Called on any loop.
	bool (*accept_proxy)(const struct ipaddr *remote_ip, void *data);
	void *data;
};

struct listener;

// Binds the listener on the pool's loop 0. tls_ctx is required for TLS
// listeners. *port is set to the bound TCP port (0 for Unix sockets).
[[nodiscard]] bool listener_start(struct loop_pool *pool, const struct listener_spec *spec,
				  SSL_CTX *tls_ctx, const struct listener_handlers *h,
				  const struct logger *log, struct listener **out, uint16_t *port,
				  struct err **err);
// Stops accepting (on loop 0); the listener is freed once its handle is
// closed
void listener_stop(struct listener *l);
// fmt_addr: "host:port", IPv4-mapped IPv6 addresses as IPv4
void fmt_addr(struct strbuf *out, const struct sockaddr *sa);
