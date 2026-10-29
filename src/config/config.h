// Configuration file loading (soju-rs config.rs)
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/err.h"
#include "base/str.h"
#include "base/vec.h"
#include "net/ipaddr.h"

#ifndef SOJU_CONFIG_PATH
#define SOJU_CONFIG_PATH ""
#endif
#ifndef SOJU_ADMIN_SOCKET_PATH
#define SOJU_ADMIN_SOCKET_PATH "/run/soju/admin"
#endif

// Default config file path ("": none)
static constexpr char CONFIG_DEFAULT_PATH[] = SOJU_CONFIG_PATH;
static constexpr char CONFIG_DEFAULT_UNIX_ADMIN_PATH[] = SOJU_ADMIN_SOCKET_PATH;

// The default and maximum chathistory-limit, the same limit as upstream
static constexpr size_t MAX_CHAT_HISTORY_LIMIT = 1000;
// The minimum chathistory-limit: clients would need too many requests to
// fetch history with a lower one
static constexpr size_t MIN_CHAT_HISTORY_LIMIT = 100;
// The largest Go time.Duration, in seconds (i64::MAX nanoseconds)
static constexpr double MAX_GO_DURATION_SECS = 9223372036854775807.0 / 1e9;

// A set of IP networks
struct ipset {
	VEC(struct ipnet) v;
};

[[nodiscard]] bool ipset_contains(const struct ipset *s, struct ipaddr ip);

enum msg_store_driver {
	MSG_STORE_MEMORY,
	MSG_STORE_DB,
	MSG_STORE_TURSO,
};

enum auth_driver {
	AUTH_INTERNAL,
	AUTH_PAM,
	AUTH_HTTP,
	AUTH_OAUTH2,
};

struct config_auth {
	enum auth_driver driver;
	char *source;
};

// Turso SQL and provisioning credentials are separate: SQL group tokens
// cannot create databases through the Platform API
struct config_turso {
	char *organization, *group, *controller, *group_token, *api_token;
};

// A duration, in nanoseconds (Rust's Duration as used in the config fits)
typedef uint64_t duration_ns;

// The subset of the configuration which can be reloaded at run time
struct config_basic {
	char *hostname;
	char *title;
	enum msg_store_driver msg_store_driver;
	char *msg_store_source;
	strvec http_origins;
	char *http_ingress;
	struct ipset accept_proxy_ips;
	bool accept_proxy_unix;
	int64_t max_user_networks;
	VEC(struct ipnet) upstream_user_ips;
	duration_ns disable_inactive_users_delay;
	duration_ns message_expiry;
	// The maximum number of messages per CHATHISTORY reply
	size_t chat_history_limit;
	bool enable_users_on_auth;
	bool client_cert_auth;
};

struct config {
	struct config_basic basic;
	strvec listen;
	bool has_tls;
	char *tls_cert_path, *tls_key_path;
	char *icon;
	char *motd_path;
	char *db_driver, *db_source;
	VEC(struct config_auth) auth;
	bool has_turso;
	struct config_turso turso;
	// Set if the config contains a file-upload directive (unsupported)
	bool file_upload;
};

// gethostname(), or "localhost"
[[nodiscard]] char *system_hostname(void);
void config_defaults(struct config *c);
void config_free(struct config *c);
[[nodiscard]] bool config_parse(struct str s, struct config *out, struct err **err);
[[nodiscard]] bool config_load(const char *filename, struct config *out, struct err **err);
// Go's net.ParseCIDR (the network, host bits cleared), with the ipnet
// crate's syntax
[[nodiscard]] bool parse_cidr(struct str s, struct ipnet *out, struct err **err);
// "<float>d": days, like upstream
[[nodiscard]] bool config_parse_duration(struct str s, duration_ns *out, struct err **err);
// Rust's f64::from_str
[[nodiscard]] bool parse_rust_f64(struct str s, double *out);
