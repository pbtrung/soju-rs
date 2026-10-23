// Risks 1 and 2: HTTP adoption after consuming exactly a PROXY header,
// metadata attached before dispatch, and asynchronous foreign-loop teardown.
#include <assert.h>
#include <libwebsockets.h>
#include <openssl/crypto.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <uv.h>

#include "support.h"

static bool received;
static char client_address[] = "192.0.2.1";

static int callback(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in,
		    size_t len) {
	(void)user;
	(void)in;
	(void)len;
	if (reason == LWS_CALLBACK_HTTP) {
		assert(lws_get_opaque_user_data(wsi) == client_address);
		received = true;
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
		.port = CONTEXT_PORT_NO_LISTEN,
		.vhost_name = "localhost",
		.protocols = protocols,
		.options = LWS_SERVER_OPTION_LIBUV | LWS_SERVER_OPTION_UV_NO_SIGSEGV_SIGFPE_SPIN,
		.foreign_loops = loops,
		.pcontext = &context,
	};
	context = lws_create_context(&info);
	assert(context != nullptr);
	struct lws_vhost *vhost = lws_get_vhost_by_name(context, "localhost");
	assert(vhost != nullptr);
	int fd[2];
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, fd) == 0);
	constexpr char proxy[] = "PROXY TCP4 192.0.2.1 192.0.2.2 1234 80\r\n";
	constexpr char request[] = "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
	assert(write(fd[1], proxy, sizeof(proxy) - 1) == (ssize_t)sizeof(proxy) - 1);
	assert(write(fd[1], request, sizeof(request) - 1) == (ssize_t)sizeof(request) - 1);
	char header[sizeof(proxy) - 1];
	assert(recv(fd[0], header, sizeof(header), MSG_WAITALL) == (ssize_t)sizeof(header));
	assert(memcmp(header, proxy, sizeof(header)) == 0);
	lws_adopt_desc_t adoption = {
		.vh = vhost,
		// lws documents this enum as an OR-able bitmask.
		// NOLINTNEXTLINE(clang-analyzer-optin.core.EnumCastOutOfRange)
		.type = (lws_adoption_type)((unsigned)LWS_ADOPT_HTTP | (unsigned)LWS_ADOPT_SOCKET),
		.fd.sockfd = fd[0],
		.opaque = client_address,
	};
	assert(lws_adopt_descriptor_vhost_via_info(&adoption) != nullptr);
	uint64_t deadline = uv_hrtime() + 5'000'000'000ULL;
	while (!received && uv_hrtime() < deadline) {
		(void)uv_run(&loop, UV_RUN_NOWAIT);
	}
	assert(received);
	assert(close(fd[1]) == 0);
	spike_shutdown(&context, &loop);
	OPENSSL_cleanup();
	return 0;
}
