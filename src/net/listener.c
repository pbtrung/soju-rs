#include "net/listener.h"

#include <errno.h>
#include <netinet/in.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/url.h"

static void free_handle(uv_handle_t *h) {
	xfree(h);
}

struct listener {
	struct ref_node ref;
	struct loop_pool *pool;
	enum listener_kind kind;
	bool unix;
	bool tls;
	SSL_CTX *tls_ctx;
	struct listener_handlers h;
	struct logger log;
	union {
		uv_tcp_t tcp;
		uv_pipe_t pipe;
	} handle;
	atomic_uint refs; // shared by the handle and sockets in transit
};

static void listener_unref(struct listener *l) {
	if (atomic_fetch_sub(&l->refs, 1) == 1) {
		if (l->tls_ctx != nullptr) {
			SSL_CTX_free(l->tls_ctx);
		}
		log_free(&l->log);
		ref_untrack(&l->ref);
		xfree(l);
	}
}

void fmt_addr(struct strbuf *out, const struct sockaddr *sa) {
	struct ipaddr ip = {0};
	uint16_t port = 0;
	if (sa->sa_family == AF_INET) {
		const struct sockaddr_in *in = (const struct sockaddr_in *)sa;
		memcpy(ip.b, &in->sin_addr, 4);
		port = ntohs(in->sin_port);
	} else if (sa->sa_family == AF_INET6) {
		const struct sockaddr_in6 *in6 = (const struct sockaddr_in6 *)sa;
		ip.v6 = true;
		memcpy(ip.b, &in6->sin6_addr, 16);
		port = ntohs(in6->sin6_port);
	}
	struct strbuf host = STRBUF_INIT;
	ipaddr_format(&host, ipaddr_canonical(ip));
	char p[8];
	snprintf(p, sizeof(p), "%u", port);
	AUTOFREE char *s = join_host_port(strbuf_cstr(&host), p);
	strbuf_append_cstr(out, s);
	strbuf_free(&host);
}

static bool sockaddr_ip(const struct sockaddr *sa, struct ipaddr *ip) {
	*ip = (struct ipaddr){0};
	if (sa->sa_family == AF_INET) {
		memcpy(ip->b, &((const struct sockaddr_in *)sa)->sin_addr, 4);
	} else if (sa->sa_family == AF_INET6) {
		ip->v6 = true;
		memcpy(ip->b, &((const struct sockaddr_in6 *)sa)->sin6_addr, 16);
	} else {
		return false;
	}
	*ip = ipaddr_canonical(*ip);
	return true;
}

// An accepted socket on its way to its loop
struct handoff {
	struct listener *l;
	uv_os_sock_t fd;
	struct sockaddr_storage remote;
	bool has_remote;
};

struct accepted {
	struct listener *l;
	bool has_remote;
	struct sockaddr_storage remote;
};

static void on_ready(struct conn *c, void *data);

static void on_closed_early(struct conn *c, const struct err *err, void *data) {
	(void)err;
	struct accepted *a = data;
	conn_set_callbacks(c, &(struct conn_callbacks){0}, nullptr);
	listener_unref(a->l);
	xfree(a);
	conn_unref(c);
}

static uint16_t sockaddr_port(const struct sockaddr *sa) {
	if (sa->sa_family == AF_INET) {
		return ntohs(((const struct sockaddr_in *)sa)->sin_port);
	}
	if (sa->sa_family == AF_INET6) {
		return ntohs(((const struct sockaddr_in6 *)sa)->sin6_port);
	}
	return 0;
}

static void build_info(struct accepted *a, struct conn *c, struct conn_info *info) {
	*info = (struct conn_info){0};
	struct ipaddr ip = {0};
	bool has_ip = a->has_remote && sockaddr_ip((const struct sockaddr *)&a->remote, &ip);
	uint16_t port = has_ip ? sockaddr_port((const struct sockaddr *)&a->remote) : 0;
	// Headers from untrusted peers are consumed but ignored
	bool trusted = a->l->h.accept_proxy != nullptr &&
		       a->l->h.accept_proxy(has_ip ? &ip : nullptr, a->l->h.data);
	info->peer_cert = conn_peer_cert(c);
	const struct proxy_header *ph = conn_proxy_header(c);
	if (trusted && ph->present) {
		if (ph->has_source) {
			ip = ipaddr_canonical(ph->source_ip);
			port = ph->source_port;
			has_ip = true;
		}
		// The proxy's client certificate replaces the TLS one, even if
		// there's none (soju-rs does the same)
		info->peer_cert = ph->has_client_cert ? &ph->client_cert : nullptr;
	}
	if (has_ip) {
		struct strbuf host = STRBUF_INIT;
		ipaddr_format(&host, ip);
		char p[8];
		snprintf(p, sizeof(p), "%u", port);
		info->remote_addr = join_host_port(strbuf_cstr(&host), p);
		strbuf_free(&host);
	} else {
		info->remote_addr = xstrdup("@");
	}
	info->has_remote_ip = has_ip;
	info->remote_ip = ip;
}

static void on_ready(struct conn *c, void *data) {
	struct accepted *a = data;
	struct listener *l = a->l;
	struct conn_info info;
	build_info(a, c, &info);
	conn_set_callbacks(c, &(struct conn_callbacks){0}, nullptr);
	switch (l->kind) {
	case LISTENER_IRC:
		if (l->h.irc != nullptr) {
			l->h.irc(c, &info, l->h.data);
			c = nullptr;
		}
		break;
	case LISTENER_ADMIN:
		if (l->h.admin != nullptr) {
			l->h.admin(c, &info, l->h.data);
			c = nullptr;
		}
		break;
	case LISTENER_HTTP:
	case LISTENER_PROMETHEUS:
		if (l->h.http != nullptr) {
			l->h.http(c, &info, l->kind == LISTENER_PROMETHEUS, l->h.data);
			c = nullptr;
		}
		break;
	default:
		break;
	}
	xfree(info.remote_addr);
	listener_unref(l);
	xfree(a);
	if (c != nullptr) {
		conn_close(c);
		conn_unref(c);
	}
}

static uv_stream_t *open_stream(uv_loop_t *loop, bool unix, uv_os_sock_t fd) {
	uv_stream_t *s;
	int rc;
	if (unix) {
		uv_pipe_t *p = xcalloc(1, sizeof(*p));
		uv_pipe_init(loop, p, 0);
		rc = uv_pipe_open(p, fd);
		s = (uv_stream_t *)p;
	} else {
		uv_tcp_t *t = xcalloc(1, sizeof(*t));
		uv_tcp_init(loop, t);
		rc = uv_tcp_open(t, fd);
		s = (uv_stream_t *)t;
	}
	if (rc != 0) {
		close(fd);
		uv_close((uv_handle_t *)s, free_handle);
		return nullptr;
	}
	return s;
}

// On the target loop
static void adopt(void *data) {
	struct handoff *ho = data;
	struct listener *l = ho->l;
	struct sloop *loop = loop_current();
	uv_stream_t *s = open_stream(&loop->uv, l->unix, ho->fd);
	if (s == nullptr) {
		listener_unref(l);
		xfree(ho);
		return;
	}
	if (l->kind == LISTENER_IDENT) {
		struct ipaddr ip;
		bool has_ip =
			ho->has_remote && sockaddr_ip((const struct sockaddr *)&ho->remote, &ip);
		struct strbuf host = STRBUF_INIT;
		if (has_ip) {
			ipaddr_format(&host, ip);
		}
		bool trusted = l->h.accept_proxy != nullptr &&
			       l->h.accept_proxy(has_ip ? &ip : nullptr, l->h.data);
		if (l->h.identd != nullptr) {
			identd_serve(l->h.identd, s, strbuf_cstr(&host), trusted);
		} else {
			uv_close((uv_handle_t *)s, free_handle);
		}
		strbuf_free(&host);
		listener_unref(l);
		xfree(ho);
		return;
	}
	struct accepted *a = xcalloc(1, sizeof(*a));
	a->l = l;
	a->has_remote = ho->has_remote;
	a->remote = ho->remote;
	xfree(ho);
	// HTTP listeners don't read PROXY headers: proxies forward addresses
	// in HTTP headers instead
	bool http = l->kind == LISTENER_HTTP || l->kind == LISTENER_PROMETHEUS;
	struct conn_options opts = {
		.tls_server = l->tls ? l->tls_ctx : nullptr,
		.proxy = !http,
		.setup_timeout_ms = DOWNSTREAM_REGISTER_TIMEOUT_MS,
		.framing = http ? CONN_FRAMING_RAW : CONN_FRAMING_LINES,
	};
	struct conn_callbacks cb = {.ready = on_ready, .closed = on_closed_early};
	(void)conn_new(s, &opts, &l->log, &cb, a);
}

static void drop_handoff(void *data) {
	struct handoff *ho = data;
	close(ho->fd);
	listener_unref(ho->l);
	xfree(ho);
}

static void on_connection(uv_stream_t *server, int status) {
	struct listener *l = server->data;
	if (status < 0) {
		struct strbuf e = STRBUF_INIT;
		io_error_str(&e, status);
		log_info(&l->log, "accept error: %s", strbuf_cstr(&e));
		strbuf_free(&e);
		return;
	}
	uv_loop_t *loop = server->loop;
	uv_stream_t *client;
	if (l->unix) {
		uv_pipe_t *p = xcalloc(1, sizeof(*p));
		uv_pipe_init(loop, p, 0);
		client = (uv_stream_t *)p;
	} else {
		uv_tcp_t *t = xcalloc(1, sizeof(*t));
		uv_tcp_init(loop, t);
		client = (uv_stream_t *)t;
	}
	if (uv_accept(server, client) != 0) {
		uv_close((uv_handle_t *)client, free_handle);
		return;
	}
	struct handoff *ho = xcalloc(1, sizeof(*ho));
	ho->l = l;
	if (!l->unix) {
		int len = sizeof(ho->remote);
		ho->has_remote = uv_tcp_getpeername((uv_tcp_t *)client,
						    (struct sockaddr *)&ho->remote, &len) == 0;
		if (l->kind == LISTENER_IRC || l->kind == LISTENER_HTTP) {
			uv_tcp_keepalive((uv_tcp_t *)client, 1, DOWNSTREAM_KEEP_ALIVE_SECS);
		}
	}
	uv_os_fd_t fd;
	if (uv_fileno((uv_handle_t *)client, &fd) != 0 || (ho->fd = dup(fd)) < 0) {
		xfree(ho);
		uv_close((uv_handle_t *)client, free_handle);
		return;
	}
	// The accepted handle stays on loop 0: its duplicate goes to the loop
	// which will own the connection
	uv_close((uv_handle_t *)client, free_handle);
	atomic_fetch_add(&l->refs, 1);
	loop_post(loop_pool_next(l->pool), adopt, drop_handoff, ho);
}

static int bind_tcp(struct listener *l, uv_loop_t *loop, const char *addr, struct err **err) {
	uv_tcp_init(loop, &l->handle.tcp);
	struct sockaddr_storage ss;
	int rc;
	if (addr[0] == ':') {
		// All interfaces, IPv4 only if IPv6 is unavailable
		unsigned long long port;
		if (!str_parse_u64(str_from(addr + 1), &port) || port > 65535) {
			err_set(err, 0, "invalid port in address %s", addr);
			return UV_EINVAL;
		}
		uv_ip6_addr("::", (int)port, (struct sockaddr_in6 *)&ss);
		rc = uv_tcp_bind(&l->handle.tcp, (const struct sockaddr *)&ss, 0);
		if (rc == UV_EAFNOSUPPORT) {
			uv_ip4_addr("0.0.0.0", (int)port, (struct sockaddr_in *)&ss);
			rc = uv_tcp_bind(&l->handle.tcp, (const struct sockaddr *)&ss, 0);
		}
		return rc;
	}
	char *host, *port;
	if (!split_host_port(str_from(addr), &host, &port, err)) {
		return UV_EINVAL;
	}
	// Like upstream, prefer the first IPv4 address of a hostname, e.g.
	// 127.0.0.1 for localhost rather than ::1
	uv_getaddrinfo_t req;
	struct addrinfo hints = {.ai_socktype = SOCK_STREAM, .ai_flags = AI_PASSIVE};
	rc = uv_getaddrinfo(loop, &req, nullptr, host[0] != '\0' ? host : nullptr, port, &hints);
	xfree(host);
	xfree(port);
	if (rc != 0) {
		return rc;
	}
	const struct addrinfo *chosen = nullptr;
	for (const struct addrinfo *ai = req.addrinfo; ai != nullptr; ai = ai->ai_next) {
		if (ai->ai_family == AF_INET) {
			chosen = ai;
			break;
		}
	}
	if (chosen == nullptr) {
		chosen = req.addrinfo;
	}
	rc = chosen != nullptr ? uv_tcp_bind(&l->handle.tcp, chosen->ai_addr, 0) : UV_EADDRNOTAVAIL;
	uv_freeaddrinfo(req.addrinfo);
	return rc;
}

static int bind_unix(struct listener *l, uv_loop_t *loop, const char *path) {
	uv_pipe_init(loop, &l->handle.pipe, 0);
	// Remove stale socket files left by a previous instance
	struct stat st;
	if (lstat(path, &st) == 0 && S_ISSOCK(st.st_mode)) {
		unlink(path);
	}
	return uv_pipe_bind(&l->handle.pipe, path);
}

static void on_listener_closed(uv_handle_t *h) {
	listener_unref(h->data);
}

bool listener_start(struct loop_pool *pool, const struct listener_spec *spec, SSL_CTX *tls_ctx,
		    const struct listener_handlers *h, const struct logger *log,
		    struct listener **out, uint16_t *port, struct err **err) {
	AUTOFREE char *label = utf8_debug_cstr(str_from(spec->label));
	if (spec->tls && tls_ctx == nullptr) {
		return err_set(err, 0, "failed to listen on %s: missing TLS configuration", label);
	}
	struct listener *l = xcalloc(1, sizeof(*l));
	REF_TRACK(&l->ref, "listener");
	l->pool = pool;
	l->kind = spec->kind;
	l->unix = spec->unix;
	l->tls = spec->tls;
	if (tls_ctx != nullptr) {
		SSL_CTX_up_ref(tls_ctx);
		l->tls_ctx = tls_ctx;
	}
	l->h = *h;
	atomic_init(&l->refs, 1);
	AUTOFREE char *prefix = xasprintf("listener %s: ", label);
	log_with_prefix(&l->log, log, prefix);
	uv_loop_t *loop = &pool->loops[0].uv;
	struct err *e = nullptr;
	int rc = spec->unix ? bind_unix(l, loop, spec->addr) : bind_tcp(l, loop, spec->addr, &e);
	l->handle.tcp.data = l;
	if (rc == 0) {
		rc = uv_listen((uv_stream_t *)&l->handle, 1024, on_connection);
	}
	if (rc != 0) {
		if (e != nullptr) {
			err_set(err, 0, "failed to start listener on %s: %s", label, err_msg(e));
			err_free(e);
		} else {
			struct strbuf m = STRBUF_INIT;
			io_error_str(&m, rc);
			err_set(err, 0, "failed to start listener on %s: %s", label,
				strbuf_cstr(&m));
			strbuf_free(&m);
		}
		uv_close((uv_handle_t *)&l->handle, on_listener_closed);
		return false;
	}
	if (spec->unix) {
		mode_t mode = spec->kind == LISTENER_ADMIN ? 0600 : 0775;
		if (chmod(spec->addr, mode) != 0) {
			if (spec->kind == LISTENER_ADMIN) {
				err_set(err, 0,
					"failed to chmod Unix admin socket: %s (os error %d)",
					strerror(errno), errno);
				uv_close((uv_handle_t *)&l->handle, on_listener_closed);
				return false;
			}
			log_info(&l->log, "failed to chmod Unix socket: %s (os error %d)",
				 strerror(errno), errno);
		}
		*port = 0;
	} else {
		struct sockaddr_storage ss;
		int len = sizeof(ss);
		uv_tcp_getsockname(&l->handle.tcp, (struct sockaddr *)&ss, &len);
		*port = ss.ss_family == AF_INET6 ? ntohs(((struct sockaddr_in6 *)&ss)->sin6_port)
						 : ntohs(((struct sockaddr_in *)&ss)->sin_port);
	}
	*out = l;
	return true;
}

void listener_stop(struct listener *l) {
	uv_close((uv_handle_t *)&l->handle, on_listener_closed);
}
