// HTTP/1.1 connections of the HTTP and Prometheus listeners (soju-rs
// listen.rs serve_http, handle_http_request, handle_websocket; it used hyper
// and tokio-tungstenite)
#include "server/http.h"

#include <stdio.h>
#include <strings.h>
#include <time.h>

#include "base/alloc.h"
#include "base/path_match.h"
#include "downstream/downstream.h"
#include "http/request.h"
#include "http/websocket.h"
#include "server/metrics.h"

// Icons are read in memory: bigger files aren't served
static constexpr size_t MAX_ICON_SIZE = (size_t)8 << 20U;

struct http_conn {
	struct ref_node ref;
	struct server *srv;
	struct conn *c; // reference; nullptr once handed to a downstream
	uv_timer_t timer; // header read timeout
	char *remote_addr;
	bool has_remote_ip;
	struct ipaddr remote_ip;
	bool has_peer_cert;
	struct strbuf peer_cert;
	bool prometheus;
	size_t body_remaining; // bytes of the current request's body to skip
	bool done; // no more requests: closing
	bool released;
};

static void on_timer_closed(uv_handle_t *h) {
	struct http_conn *hc = h->data;
	conn_unref(hc->c);
	xfree(hc->remote_addr);
	strbuf_free(&hc->peer_cert);
	ref_untrack(&hc->ref);
	xfree(hc);
}

// Drops the HTTP state (the connection is closed or upgraded)
static void release(struct http_conn *hc) {
	if (hc->released) {
		return;
	}
	hc->released = true;
	uv_timer_stop(&hc->timer);
	uv_close((uv_handle_t *)&hc->timer, on_timer_closed);
}

static void on_header_timeout(uv_timer_t *t) {
	struct http_conn *hc = t->data;
	hc->done = true;
	conn_close(hc->c);
}

static void start_timer(struct http_conn *hc) {
	uv_timer_start(&hc->timer, on_header_timeout, HTTP_HEADER_READ_TIMEOUT_MS, 0);
}

static const char *reason(int status) {
	switch (status) {
	case 101:
		return "Switching Protocols";
	case 200:
		return "OK";
	case 400:
		return "Bad Request";
	case 403:
		return "Forbidden";
	case 404:
		return "Not Found";
	case 405:
		return "Method Not Allowed";
	case 426:
		return "Upgrade Required";
	case 431:
		return "Request Header Fields Too Large";
	default:
		return "";
	}
}

// The status line and the headers every response has
static void begin_response(struct strbuf *out, int status) {
	strbuf_printf(out, "HTTP/1.1 %d %s\r\n", status, reason(status));
	char date[64];
	time_t now = time(nullptr);
	struct tm tm;
	if (gmtime_r(&now, &tm) != nullptr &&
	    strftime(date, sizeof(date), "%a, %d %b %Y %H:%M:%S GMT", &tm) > 0) {
		strbuf_printf(out, "date: %s\r\n", date);
	}
}

// Sends a complete response; body may hold NUL bytes
static void respond(struct http_conn *hc, const struct http_request *req, int status,
		    const char *content_type, struct str body, bool close) {
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	begin_response(&out, status);
	if (content_type != nullptr) {
		strbuf_printf(&out, "content-type: %s\r\n", content_type);
	}
	strbuf_printf(&out, "content-length: %zu\r\n", body.len);
	if (close) {
		strbuf_append_cstr(&out, "connection: close\r\n");
	}
	strbuf_append(&out, "\r\n", 2);
	if (req == nullptr || strcmp(req->method, "HEAD") != 0) {
		strbuf_append_str(&out, body);
	}
	conn_send_raw(hc->c, out.buf, out.len);
}

static void not_found(struct http_conn *hc, const struct http_request *req) {
	respond(hc, req, 404, nullptr, STR_LIT("404 page not found\n"), false);
}

static const char *content_type_for(const char *path) {
	const char *dot = strrchr(path, '.');
	if (dot == nullptr) {
		return nullptr;
	}
	static const char *const types[][2] = {
		{"png", "image/png"},   {"svg", "image/svg+xml"}, {"ico", "image/x-icon"},
		{"jpg", "image/jpeg"},  {"jpeg", "image/jpeg"},   {"gif", "image/gif"},
		{"webp", "image/webp"},
	};
	for (size_t i = 0; i < sizeof(types) / sizeof(types[0]); i++) {
		if (strcasecmp(dot + 1, types[i][0]) == 0) {
			return types[i][1];
		}
	}
	return nullptr;
}

static bool read_file(const char *path, struct strbuf *out) {
	FILE *f = fopen(path, "rb");
	if (f == nullptr) {
		return false;
	}
	char buf[16384];
	bool ok = true;
	for (;;) {
		size_t n = fread(buf, 1, sizeof(buf), f);
		if (out->len + n > MAX_ICON_SIZE) {
			ok = false;
			break;
		}
		strbuf_append(out, buf, n);
		if (n < sizeof(buf)) {
			ok = ferror(f) == 0;
			break;
		}
	}
	fclose(f);
	return ok;
}

static void serve_icon(struct http_conn *hc, const struct http_request *req) {
	struct server_config *cfg = server_config_get(hc->srv);
	AUTO_STRBUF struct strbuf data = STRBUF_INIT;
	if (cfg->icon_path[0] == '\0' || !read_file(cfg->icon_path, &data)) {
		not_found(hc, req);
	} else {
		respond(hc, req, 200, content_type_for(cfg->icon_path), strbuf_str(&data), false);
	}
	server_config_unref(cfg);
}

static void serve_metrics(struct http_conn *hc, const struct http_request *req) {
	AUTO_STRBUF struct strbuf body = STRBUF_INIT;
	server_render_metrics(hc->srv, &body);
	respond(hc, req, 200, "text/plain; version=0.0.4; charset=utf-8", strbuf_str(&body), false);
}

// The checks of the WebSocket handshake, in soju-rs's order: an error
// response, or 0
static int check_upgrade(const struct http_request *req, const char **body) {
	if (!http_header_has_token(req, "Connection", "upgrade")) {
		*body = "WebSocket protocol violation: Connection header \"\" does not contain "
			"Upgrade\n";
		return 426;
	}
	if (!http_header_has_token(req, "Upgrade", "websocket")) {
		*body = "WebSocket protocol violation: Upgrade header does not contain websocket\n";
		return 426;
	}
	if (strcmp(req->method, "GET") != 0) {
		*body = "WebSocket protocol violation: handshake request method is not GET\n";
		return 405;
	}
	const char *version = http_header(req, "Sec-WebSocket-Version");
	if (version == nullptr || strcmp(version, "13") != 0) {
		*body = "unsupported WebSocket protocol version (only 13 is supported)\n";
		return 400;
	}
	if (http_header(req, "Sec-WebSocket-Key") == nullptr) {
		*body = "WebSocket protocol violation: missing Sec-WebSocket-Key\n";
		return 400;
	}
	return 0;
}

// Subprotocol negotiation: "text.ircv3.net" (non-compliant, fight me)
static bool wants_ircv3_subprotocol(const struct http_request *req) {
	return http_header_has_token(req, "Sec-WebSocket-Protocol", "text.ircv3.net");
}

// Only trust forwarding headers from a trusted proxy IP, to prevent users
// from spoofing the remote address. Unix sockets have no remote address:
// proxy headers are ignored there.
static void proxied_info(struct http_conn *hc, const struct http_request *req,
			 const struct server_config *cfg, char **remote_addr, struct strbuf *cert,
			 bool *has_cert) {
	*remote_addr = xstrdup(hc->remote_addr);
	*has_cert = hc->has_peer_cert;
	if (hc->has_peer_cert) {
		strbuf_append_str(cert, strbuf_str(&hc->peer_cert));
	}
	if (!hc->has_remote_ip ||
	    !ipset_contains(&cfg->cfg.basic.accept_proxy_ips, hc->remote_ip)) {
		return;
	}
	struct header_values fwd = http_header_values(req, "Forwarded");
	struct header_values xff = http_header_values(req, "X-Forwarded-For");
	char *f = parse_forwarded(fwd, xff);
	http_header_values_free(fwd);
	http_header_values_free(xff);
	if (f != nullptr) {
		xfree(*remote_addr);
		*remote_addr = f;
	}
	const char *cc = http_header(req, "Client-Cert");
	if (cc != nullptr && cc[0] != '\0') {
		strbuf_reset(cert);
		*has_cert = parse_client_cert(str_from(cc), cert);
	}
}

// Upgrades to WebSocket and hands the connection to a downstream. Returns
// false if a response was sent instead.
static bool serve_websocket(struct http_conn *hc, const struct http_request *req) {
	const char *body = nullptr;
	int status = check_upgrade(req, &body);
	if (status != 0) {
		respond(hc, req, status, nullptr, str_from(body), false);
		return false;
	}
	struct server_config *cfg = server_config_get(hc->srv);
	struct err *e = nullptr;
	if (!authenticate_origin(http_header(req, "Origin"), http_header(req, "Host"),
				 &cfg->cfg.basic.http_origins, &e)) {
		log_info(&hc->srv->log, "failed to serve HTTP connection: %s", err_msg(e));
		AUTOFREE char *msg = xasprintf("%s\n", err_msg(e));
		respond(hc, req, 403, nullptr, str_from(msg), false);
		err_free(e);
		server_config_unref(cfg);
		return false;
	}
	char *remote_addr;
	AUTO_STRBUF struct strbuf cert = STRBUF_INIT;
	bool has_cert;
	proxied_info(hc, req, cfg, &remote_addr, &cert, &has_cert);
	server_config_unref(cfg);

	AUTOFREE char *accept = ws_accept_key(str_from(http_header(req, "Sec-WebSocket-Key")));
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	begin_response(&out, 101);
	strbuf_printf(&out,
		      "connection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: %s\r\n",
		      accept);
	if (wants_ircv3_subprotocol(req)) {
		strbuf_append_cstr(&out, "sec-websocket-protocol: text.ircv3.net\r\n");
	}
	strbuf_append(&out, "\r\n", 2);
	conn_send_raw(hc->c, out.buf, out.len);
	conn_set_framing(hc->c, CONN_FRAMING_WEBSOCKET);

	// Like upstream, whose WebSocket connections don't have a TCP address:
	// device certificates record no IP for them
	struct conn_info info = {.remote_addr = remote_addr,
				 .peer_cert = has_cert ? &cert : nullptr};
	struct conn *c = hc->c;
	struct server *srv = hc->srv;
	hc->c = nullptr;
	hc->done = true;
	release(hc);
	downstream_serve(srv, c, &info, nullptr);
	xfree(remote_addr);
	return true;
}

// Returns false once the connection was handed over
static bool handle_request(struct http_conn *hc, const struct http_request *req) {
	if (hc->prometheus) {
		serve_metrics(hc, req);
	} else if (str_eq_cstr(req->path, "/socket")) {
		if (serve_websocket(hc, req)) {
			return false;
		}
	} else if (str_eq_cstr(req->path, "/icon")) {
		serve_icon(hc, req);
	} else {
		not_found(hc, req);
	}
	return true;
}

static void finish(struct http_conn *hc) {
	hc->done = true;
	uv_timer_stop(&hc->timer);
	conn_shutdown(hc->c);
}

static void on_raw(struct conn *c, struct strbuf *in, bool eof, void *data) {
	(void)c;
	struct http_conn *hc = data;
	if (hc->done) {
		// Closing: the rest is ignored
		strbuf_reset(in);
		return;
	}
	while (!hc->done) {
		if (hc->body_remaining > 0) {
			size_t n = in->len < hc->body_remaining ? in->len : hc->body_remaining;
			strbuf_drop_front(in, n);
			hc->body_remaining -= n;
			if (hc->body_remaining > 0) {
				break;
			}
		}
		struct http_request req;
		size_t used = 0;
		enum http_parse_result r = http_parse_request(strbuf_str(in), &req, &used);
		if (r == HTTP_NEED_MORE) {
			break;
		}
		if (r == HTTP_BAD_REQUEST || r == HTTP_HEADERS_TOO_LARGE) {
			int status = r == HTTP_BAD_REQUEST ? 400 : 431;
			respond(hc, nullptr, status, nullptr, STR_EMPTY, true);
			finish(hc);
			return;
		}
		strbuf_drop_front(in, used);
		uv_timer_stop(&hc->timer);
		hc->body_remaining = req.content_length;
		bool keep = req.keep_alive && !req.chunked;
		bool more = handle_request(hc, &req);
		http_request_free(&req);
		if (!more) {
			return;
		}
		if (!keep) {
			finish(hc);
			return;
		}
		start_timer(hc);
	}
	if (eof && !hc->done) {
		hc->done = true;
		conn_close(hc->c);
	}
}

static void on_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	(void)err;
	release(data);
}

void http_serve(struct server *srv, struct conn *c, const struct conn_info *info, bool prometheus) {
	struct http_conn *hc = xcalloc(1, sizeof(*hc));
	REF_TRACK(&hc->ref, "http_conn");
	hc->srv = srv;
	hc->c = c;
	hc->prometheus = prometheus;
	hc->remote_addr = xstrdup(info->remote_addr);
	hc->has_remote_ip = info->has_remote_ip;
	hc->remote_ip = info->remote_ip;
	if (info->peer_cert != nullptr) {
		hc->has_peer_cert = true;
		strbuf_append_str(&hc->peer_cert, strbuf_str(info->peer_cert));
	}
	uv_timer_init(conn_stream(c)->loop, &hc->timer);
	hc->timer.data = hc;
	start_timer(hc);
	conn_set_framing(c, CONN_FRAMING_RAW);
	conn_set_callbacks(c, &(struct conn_callbacks){.raw = on_raw, .closed = on_closed}, hc);
	if (conn_is_closed(c)) {
		release(hc);
	}
}
