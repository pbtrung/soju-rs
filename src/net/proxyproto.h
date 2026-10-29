// PROXY protocol (v1 and v2) parsing (soju-rs proxyproto.rs), see
// https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt
//
// Incremental: the caller passes everything read so far. Like soju-rs, a
// connection which doesn't start with a PROXY signature is never made to
// wait: as soon as the bytes stop matching a signature, they belong to the
// stream.
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/str.h"
#include "net/ipaddr.h"

struct proxy_header {
	bool present; // a header was read
	// Unset for LOCAL/UNKNOWN connections
	bool has_source;
	struct ipaddr source_ip, destination_ip;
	uint16_t source_port, destination_port;
	// DER client certificate from a PP2_TYPE_SSL TLV
	bool has_client_cert;
	struct strbuf client_cert;
};

void proxy_header_free(struct proxy_header *h);

enum proxy_result {
	PROXY_NEED_MORE, // the bytes so far may start a header
	PROXY_NONE, // no header: every byte belongs to the stream
	PROXY_HEADER, // a header of *consumed bytes, the rest is the stream
	PROXY_ERROR,
};

// eof: no more bytes will come
[[nodiscard]] enum proxy_result proxy_parse(struct str buf, bool eof, struct proxy_header *h,
					    size_t *consumed, struct err **err);
