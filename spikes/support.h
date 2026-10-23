#pragma once

#include <assert.h>
#include <libwebsockets.h>
#include <uv.h>

static inline void spike_shutdown(struct lws_context **context, uv_loop_t *loop) {
	lws_context_destroy(*context);
	(void)uv_run(loop, UV_RUN_DEFAULT);
	// lws 5 defers FINALIZATION on a foreign loop to an explicit final call.
	if (*context != nullptr) {
		lws_context_destroy(*context);
	}
	assert(*context == nullptr);
	assert(uv_loop_close(loop) == 0);
}
