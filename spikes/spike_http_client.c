// Risk 4: resolve and validate outside lws, then pin a numeric address
// while retaining the original HTTP Host / TLS verification name. Never
// let lws follow redirects: each redirect must pass the address policy.
// A request the server never answers is killed by our own timer: lws
// re-arms its own timeout with the context-wide timeout_secs at each stage
// of a client connection, so a per-request lws_set_timeout() doesn't
// last. Production code arms a libuv timer per request and kills the
// connection with LWS_TO_KILL_SYNC, which is safe outside lws callbacks
// (LWS_TO_KILL_ASYNC never closed it here). Rust's reqwest timeouts are
// totals per request too.
#include <assert.h>
#include <libwebsockets.h>
#include <openssl/crypto.h>
#include <string.h>
#include <uv.h>

#include "support.h"

static bool requested, completed, slow_requested, timed_out;
static struct lws *slow_wsi;

static void on_request_timeout(uv_timer_t *t) {
	(void)t;
	if (slow_wsi != nullptr) {
		lws_set_timeout(slow_wsi, PENDING_TIMEOUT_USER_OK, LWS_TO_KILL_SYNC);
	}
}

static int callback(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in,
		    size_t len) {
	(void)user;
	(void)len;
	if (reason == LWS_CALLBACK_HTTP && strcmp((const char *)in, "/slow") == 0) {
		// Never answer
		slow_requested = true;
		return 0;
	}
	if (reason == LWS_CALLBACK_HTTP) {
		char host[128];
		assert(lws_hdr_copy(wsi, host, sizeof(host), WSI_TOKEN_HOST) > 0);
		assert(strcmp(host, "pinning.invalid") == 0);
		requested = true;
		return lws_return_http_status(wsi, HTTP_STATUS_OK, "ok");
	}
	if (lws_get_opaque_user_data(wsi) == &slow_wsi &&
	    (reason == LWS_CALLBACK_CLIENT_CONNECTION_ERROR ||
	     reason == LWS_CALLBACK_CLOSED_CLIENT_HTTP)) {
		timed_out = true;
		slow_wsi = nullptr;
		return 0;
	}
	if (reason == LWS_CALLBACK_ESTABLISHED_CLIENT_HTTP) {
		assert(lws_http_client_http_response(wsi) == HTTP_STATUS_OK);
		completed = true;
		return -1;
	}
	return 0;
}

int main(void) {
	uv_loop_t loop;
	assert(uv_loop_init(&loop) == 0);
	void *loops[] = {&loop};
	struct lws_context *context = nullptr;
	const struct lws_protocols protocols[] = {{.name = "http", .callback = callback}, {0}};
	struct lws_context_creation_info info = {
		.port = 0,
		.iface = "127.0.0.1",
		.vhost_name = "pinning.invalid",
		.protocols = protocols,
		.options = LWS_SERVER_OPTION_LIBUV | LWS_SERVER_OPTION_UV_NO_SIGSEGV_SIGFPE_SPIN,
		.foreign_loops = loops,
		.pcontext = &context,
	};
	context = lws_create_context(&info);
	assert(context != nullptr);
	struct lws_vhost *vhost = lws_get_vhost_by_name(context, "pinning.invalid");
	assert(vhost != nullptr);
	struct lws_client_connect_info client = {
		.context = context,
		.address = "127.0.0.1", // test exception: production rejects loopback
		.host = "pinning.invalid",
		.port = lws_get_vhost_listen_port(vhost),
		.path = "/",
		.method = "GET",
		.local_protocol_name = "http",
		.ssl_connection = LCCSCF_HTTP_NO_FOLLOW_REDIRECT,
	};
	struct lws *wsi = lws_client_connect_via_info(&client);
	assert(wsi != nullptr);
	lws_set_timeout(wsi, PENDING_TIMEOUT_AWAITING_SERVER_RESPONSE, 5);
	uint64_t deadline = uv_hrtime() + 5'000'000'000ULL;
	while (!completed && uv_hrtime() < deadline) {
		(void)uv_run(&loop, UV_RUN_NOWAIT);
	}
	assert(requested && completed);

	client.path = "/slow";
	// lws clears *pwsi before the close callbacks: tell the request
	// apart by its opaque pointer
	client.opaque_user_data = &slow_wsi;
	wsi = lws_client_connect_via_info(&client);
	assert(wsi != nullptr);
	slow_wsi = wsi;
	uv_timer_t timer;
	assert(uv_timer_init(&loop, &timer) == 0);
	assert(uv_timer_start(&timer, on_request_timeout, 1000, 0) == 0);
	uint64_t start = uv_hrtime();
	deadline = start + 10'000'000'000ULL;
	while (!timed_out && uv_hrtime() < deadline) {
		(void)uv_run(&loop, UV_RUN_NOWAIT);
	}
	assert(slow_requested && timed_out);
	assert(uv_hrtime() - start < 3'000'000'000ULL);
	uv_close((uv_handle_t *)&timer, nullptr);
	spike_shutdown(&context, &loop);
	OPENSSL_cleanup();
	return 0;
}
