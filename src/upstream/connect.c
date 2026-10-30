#include "upstream/internal.h"

#include <netdb.h>

#include "base/alloc.h"
#include "net/tls.h"
#include "net/listener.h"
#include "user/user.h"
#include "net/url.h"

struct dial {
	uint64_t id, generation;
	uv_getaddrinfo_t dns;
	uv_connect_t connect;
	uv_stream_t *stream;
	struct addrinfo *addresses, *next;
	char *host, *port;
	char address[64]; // the address being tried, for errors
	bool tls;
};

struct input {
	uint64_t id, generation;
	struct conn *conn; // reference, used only on its owning loop
	struct irc_msg *msg;
};

static void input_done(void *data) {
	struct input *in = data;
	irc_msg_free(in->msg);
	conn_input_done(in->conn);
	conn_unref(in->conn);
	xfree(in);
}

static void input_run(void *data) {
	struct input *in = data;
	struct upstream *uc = upstream_lookup(in->id);
	if (uc != nullptr && uc->generation == in->generation && uc->conn == in->conn) {
		struct irc_msg *msg = in->msg;
		in->msg = nullptr;
		upstream_receive_take(uc, msg);
	}
	input_done(in);
}

static void message(struct conn *c, struct irc_msg *msg, void *data) {
	struct upstream *uc = upstream_lookup((uintptr_t)data);
	if (uc == nullptr || uc->conn != c) {
		irc_msg_free(msg);
		conn_input_done(c);
		return;
	}
	struct input *in = xcalloc(1, sizeof(*in));
	*in = (struct input){.id = uc->id, .generation = uc->generation, .conn = c, .msg = msg};
	conn_ref(c);
	loop_post(uc->loop, input_run, input_done, in);
}

// soju-rs user::upstream_closed: a registered connection closed without
// a read error has no error
static void closed(struct conn *c, const struct err *err, void *data) {
	struct upstream *uc = upstream_lookup((uintptr_t)data);
	if (uc == nullptr || uc->conn != c) {
		return;
	}
	AUTOFREE char *reason =
		!uc->registered ? xasprintf("failed to register: failed to read message: %s",
					    err != nullptr ? err_msg(err) : "EOF")
		: err != nullptr
			? xasprintf("failed to handle messages: failed to read IRC command: %s",
				    err_msg(err))
			: xstrdup("");
	upstream_disconnect(uc, reason, true);
}

static void ready(struct conn *c, void *data) {
	struct upstream *uc = upstream_lookup((uintptr_t)data);
	if (uc != nullptr && uc->conn == c) {
		uc->now = uv_now(&uc->loop->uv);
		upstream_register(uc);
	}
}

static const struct conn_callbacks callbacks = {
	.ready = ready, .message = message, .closed = closed};

void upstream_adopt(struct upstream *uc, struct conn *c) {
	if (uc->conn != nullptr) {
		conn_set_callbacks(uc->conn, &(struct conn_callbacks){0}, nullptr);
		conn_close(uc->conn);
		conn_unref(uc->conn);
	}
	uc->generation++;
	upstream_clear_session(uc);
	uc->conn = c;
	upstream_count(uc, true);
	conn_set_callbacks(c, &callbacks, (void *)(uintptr_t)uc->id);
	upstream_register(uc);
}

static void stream_closed(uv_handle_t *h) {
	xfree(h);
}

static void dial_free(struct dial *d) {
	if (d->addresses != nullptr) {
		uv_freeaddrinfo(d->addresses);
	}
	xfree(d->host);
	xfree(d->port);
	xfree(d);
}

static void try_address(struct dial *d);

static void connected(uv_connect_t *req, int status) {
	struct dial *d = req->data;
	struct upstream *uc = upstream_lookup(d->id);
	if (uc == nullptr || uc->generation != d->generation || uc->closing) {
		uv_close((uv_handle_t *)d->stream, stream_closed);
		dial_free(d);
		return;
	}
	if (status < 0) {
		uv_close((uv_handle_t *)d->stream, stream_closed);
		d->stream = nullptr;
		if (d->next != nullptr) {
			try_address(d);
			return;
		}
		AUTOFREE char *errno_text = go_errno_message(status);
		AUTOFREE char *reason = xasprintf("failed to connect: dial tcp %s: connect: %s",
						  d->address, errno_text);
		upstream_disconnect(uc, reason, true);
		dial_free(d);
		return;
	}
	if (d->stream->type == UV_TCP) {
		int ret = uv_tcp_keepalive((uv_tcp_t *)d->stream, 1, 15);
		if (ret < 0) {
			log_info(&uc->log, "failed to enable TCP keep-alive: %s", uv_strerror(ret));
		}
	}
	if (uc->srv->identd != nullptr && d->stream->type == UV_TCP) {
		struct sockaddr_storage remote, local;
		int rlen = sizeof(remote), llen = sizeof(local);
		if (uv_tcp_getpeername((uv_tcp_t *)d->stream, (struct sockaddr *)&remote, &rlen) ==
			    0 &&
		    uv_tcp_getsockname((uv_tcp_t *)d->stream, (struct sockaddr *)&local, &llen) ==
			    0) {
			struct strbuf r = STRBUF_INIT, l = STRBUF_INIT;
			fmt_addr(&r, (const struct sockaddr *)&remote);
			fmt_addr(&l, (const struct sockaddr *)&local);
			uc->ident_remote = strbuf_take(&r);
			uc->ident_local = strbuf_take(&l);
			AUTOFREE char *ident = user_ident(uc->user.id);
			identd_store(uc->srv->identd, uc->ident_remote, uc->ident_local, ident);
		}
	}
	struct err *err = nullptr;
	SSL_CTX *tls = nullptr;
	if (d->tls) {
		tls = tls_client_ctx(uc->network.certfp,
				     strbuf_str(&uc->network.sasl.external_cert.data),
				     strbuf_str(&uc->network.sasl.external_key.data), &err);
		if (tls == nullptr) {
			uv_close((uv_handle_t *)d->stream, stream_closed);
			AUTOFREE char *reason = xasprintf("failed to connect: %s", err_msg(err));
			upstream_disconnect(uc, reason, false);
			err_free(err);
			dial_free(d);
			return;
		}
	}
	struct conn_rate rate = {
		.delay_ns = 2000000000ULL, .burst = 10, .enabled = &uc->rate_enabled};
	struct conn_options opts = {.tls_client = tls,
				    .tls_host = d->host,
				    .setup_timeout_ms = UPSTREAM_CONNECT_TIMEOUT_MS,
				    .rate = &rate};
	uc->conn = conn_new(d->stream, &opts, &uc->log, &callbacks, (void *)(uintptr_t)uc->id);
	SSL_CTX_free(tls);
	dial_free(d);
}

static void try_address(struct dial *d) {
	struct upstream *uc = upstream_lookup(d->id);
	if (uc == nullptr || uc->generation != d->generation) {
		dial_free(d);
		return;
	}
	struct addrinfo *addr = d->next;
	d->next = addr->ai_next;
	char ip[INET6_ADDRSTRLEN] = "";
	if (addr->ai_family == AF_INET6) {
		const struct sockaddr_in6 *in6 = (const struct sockaddr_in6 *)addr->ai_addr;
		uv_ip6_name(in6, ip, sizeof(ip));
		snprintf(d->address, sizeof(d->address), "[%s]:%u", ip, ntohs(in6->sin6_port));
	} else {
		const struct sockaddr_in *in = (const struct sockaddr_in *)addr->ai_addr;
		uv_ip4_name(in, ip, sizeof(ip));
		snprintf(d->address, sizeof(d->address), "%s:%u", ip, ntohs(in->sin_port));
	}
	uv_tcp_t *tcp = xcalloc(1, sizeof(*tcp));
	uv_tcp_init(&uc->loop->uv, tcp);
	d->stream = (uv_stream_t *)tcp;
	d->connect.data = d;
	int ret = uv_tcp_connect(&d->connect, tcp, addr->ai_addr, connected);
	if (ret < 0) {
		connected(&d->connect, ret);
	}
}

static void resolved(uv_getaddrinfo_t *req, int status, struct addrinfo *addresses) {
	struct dial *d = req->data;
	d->addresses = d->next = addresses;
	struct upstream *uc = upstream_lookup(d->id);
	if (uc == nullptr || uc->generation != d->generation) {
		dial_free(d);
		return;
	}
	if (status < 0 || addresses == nullptr) {
		AUTOFREE char *reason = xasprintf(
			"failed to connect: %s", status < 0 ? uv_strerror(status) : "no addresses");
		upstream_disconnect(uc, reason, true);
		dial_free(d);
		return;
	}
	try_address(d);
}

void upstream_connect(struct upstream *uc) {
	if (uc->closing || uc->conn != nullptr || uc->state == UPSTREAM_CONNECTING) {
		return;
	}
	uc->reconnect = true;
	uc->generation++;
	uc->now = uv_now(&uc->loop->uv);
	uc->state = UPSTREAM_CONNECTING;
	uc->last_try = uc->now;
	upstream_count(uc, true);
	uc->deadline = uc->now + UPSTREAM_CONNECT_TIMEOUT_MS;
	struct url url = {0};
	struct err *err = nullptr;
	if (!network_url(uc->network.addr, &url, &err)) {
		AUTOFREE char *reason = xasprintf("failed to connect: %s", err_msg(err));
		upstream_disconnect(uc, reason, false);
		err_free(err);
		return;
	}
	struct dial *d = xcalloc(1, sizeof(*d));
	d->id = uc->id;
	d->generation = uc->generation;
	d->tls = cstr_eq(url.scheme, "ircs");
	if (cstr_eq(url.scheme, "irc+unix") || cstr_eq(url.scheme, "unix")) {
		uv_pipe_t *pipe = xcalloc(1, sizeof(*pipe));
		uv_pipe_init(&uc->loop->uv, pipe, 0);
		d->stream = (uv_stream_t *)pipe;
		d->connect.data = d;
		d->host = xstrdup("");
		d->port = xstrdup("");
		uv_pipe_connect(&d->connect, pipe, url.path, connected);
		url_free(&url);
		return;
	}
	if (!d->tls && !cstr_eq(url.scheme, "irc+insecure")) {
		url_free(&url);
		dial_free(d);
		upstream_disconnect(uc, "failed to connect: unsupported upstream scheme", false);
		return;
	}
	if (!split_host_port(str_from(url.host), &d->host, &d->port, nullptr)) {
		d->host = xstrdup(url.host);
		d->port = xstrdup(d->tls ? "6697" : "6667");
	}
	url_free(&url);
	struct addrinfo hints = {
		.ai_family = AF_UNSPEC, .ai_socktype = SOCK_STREAM, .ai_protocol = IPPROTO_TCP};
	d->dns.data = d;
	int ret = uv_getaddrinfo(&uc->loop->uv, &d->dns, resolved, d->host, d->port, &hints);
	if (ret < 0) {
		resolved(&d->dns, ret, nullptr);
	}
}
