// Risk 4: resolve and validate outside lws, then pin a numeric address
// while retaining the original HTTP Host / TLS verification name. Never
// let lws follow redirects: each redirect must pass the address policy.
#include <assert.h>
#include <libwebsockets.h>
#include <openssl/crypto.h>
#include <string.h>
#include <uv.h>

#include "support.h"

static bool requested, completed;

static int callback(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in,
		    size_t len) {
	(void)user;
	(void)in;
	(void)len;
	if (reason == LWS_CALLBACK_HTTP) {
		char host[128];
		assert(lws_hdr_copy(wsi, host, sizeof(host), WSI_TOKEN_HOST) > 0);
		assert(strcmp(host, "pinning.invalid") == 0);
		requested = true;
		return lws_return_http_status(wsi, HTTP_STATUS_OK, "ok");
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
	spike_shutdown(&context, &loop);
	OPENSSL_cleanup();
	return 0;
}
