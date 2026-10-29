// An Identification Protocol server, RFC 1413 (soju-rs identd.rs). The
// table of idents is shared by the loops.
#pragma once

#include <pthread.h>
#include <stdint.h>
#include <uv.h>

#include "base/map.h"

struct identd {
	pthread_mutex_t mu;
	struct strmap entries; // "host\nremote_port\nlocal_port" -> ident
};

void identd_init(struct identd *id);
void identd_finish(struct identd *id);
// Addresses are "host:port"; invalid ones are ignored
void identd_store(struct identd *id, const char *remote_addr, const char *local_addr,
		  const char *ident);
void identd_delete(struct identd *id, const char *remote_addr, const char *local_addr);
// Serves one ident client on stream (owned: closed and freed when done).
// remote_host is the client's IP. A PROXY header is read first; its source
// replaces remote_host when trust_proxy (an untrusted one is ignored).
void identd_serve(struct identd *id, uv_stream_t *stream, const char *remote_host,
		  bool trust_proxy);
// "local , remote" -> ports; false if invalid (exposed for tests)
[[nodiscard]] bool identd_parse_query(const char *l, uint16_t *local, uint16_t *remote);
