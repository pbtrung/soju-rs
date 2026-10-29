// Minimal URL and host:port handling following Go's net/url and net, as
// soju-rs does (database/mod.rs)
#pragma once

#include "base/err.h"
#include "base/str.h"

// scheme://[userinfo@]host[:port][/path][?query][#fragment]; every field
// is an owned string
struct url {
	char *scheme; // lowercased
	char *host;
	char *path;
	bool has_user;
	char *query;
	char *fragment;
};

[[nodiscard]] bool url_parse(struct str s, struct url *out, struct err **err);
void url_free(struct url *u);
// Network::url: a raw domain name gets the ircs scheme; for irc+unix and
// unix, the host is part of the path
[[nodiscard]] bool network_url(const char *addr, struct url *out, struct err **err);

// Go's net.SplitHostPort; host and port are new strings
[[nodiscard]] bool split_host_port(struct str hostport, char **host, char **port, struct err **err);
// Go's net.JoinHostPort
[[nodiscard]] char *join_host_port(const char *host, const char *port);
