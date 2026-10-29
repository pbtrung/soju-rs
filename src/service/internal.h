#pragma once
#include "service/service.h"
#include "base/alloc.h"
#include "base/utf8.h"
#include "config/flags.h"
#include "user/internal.h"

struct service_context {
	struct server *srv;
	struct sloop *origin;
	int64_t user_id, reply_user, network_id;
	uint64_t dc_id, deadline;
	bool admin;
	atomic_uint refs;
	unsigned depth;
	char *nick, *command;
	struct strbuf peer_cert;
	bool has_remote_ip;
	struct ipaddr remote_ip;
	service_print_fn print;
	service_done_fn done;
	void *data;
	struct err *err;
};
#define SFAIL(c, ...) err_set(&(c)->err, 0, __VA_ARGS__)
void service_print(struct service_context *c, const char *text);
[[gnu::format(printf, 2, 3)]] void service_printf(struct service_context *c, const char *fmt, ...);
void service_finish(struct service_context *c);
void service_run(struct user *u, struct service_context *c, const char *const *words, size_t n);
void service_drop(void *data);
bool service_parse(struct service_context *c, struct flags_parsed *p, const char *spec,
		   const char *const *args, size_t n);
bool service_no_args(struct service_context *c, const struct flags_parsed *p);
struct user_network *service_network(struct user *u, struct service_context *c, const char *name);
void service_replace(char **dst, const char *value);
char *service_hex(struct str bytes, bool colon);
bool service_unhex(const char *s, struct strbuf *out);
char *service_digest(const char *name, struct str bytes);
#define HANDLER(name)                                                                          \
	int service_##name(struct user *u, struct service_context *c, const char *const *args, \
			   size_t n)
HANDLER(handle_help);
HANDLER(network_create);
HANDLER(network_status);
HANDLER(network_info);
HANDLER(network_update);
HANDLER(network_delete);
HANDLER(network_quote);
HANDLER(channel_status);
HANDLER(channel_create);
HANDLER(channel_update);
HANDLER(channel_delete);
HANDLER(certfp_generate);
HANDLER(certfp_fingerprints);
HANDLER(sasl_status);
HANDLER(sasl_set_plain);
HANDLER(sasl_reset);
HANDLER(device_cert_status);
HANDLER(device_cert_create);
HANDLER(device_cert_delete);
HANDLER(user_status);
HANDLER(user_create);
HANDLER(user_update);
HANDLER(user_delete);
HANDLER(user_run);
HANDLER(server_status);
HANDLER(server_notice);
HANDLER(server_debug);

static inline const char *service_flag_string(const struct flags_parsed *p, const char *name,
					      const char *fallback) {
	const char *v = flags_get(p, name);
	return v != nullptr ? v : fallback;
}

enum { SERVICE_DEFERRED = 2 };
// Handlers returning SERVICE_DEFERRED must reserve a reference before posting
// a continuation. Dispatch releases its own reference only after they return.
void service_defer(struct service_context *c);

extern const struct service_command service_commands[];
extern const size_t service_commands_len;
