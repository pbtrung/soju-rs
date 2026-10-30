// BouncerServ command dispatch. Deferred work owns snapshots and addresses
// actors by ID, so a disconnected client or deleted user can disappear safely.
#pragma once
#include "user/user.h"

struct service_context;
struct service_command {
	const char *name, *usage, *desc;
	bool admin, global;
	const struct service_command *children;
	size_t children_len;
	int (*handle)(struct user *, struct service_context *, const char *const *, size_t);
};
// Lookup accepts unambiguous abbreviations, and consumes only command words.
[[nodiscard]] const struct service_command *service_lookup(const char *const *words, size_t n,
							   size_t *used, struct err **err);
void service_help(bool admin, bool global, strvec *out);
[[nodiscard]] bool service_parse_duration(const char *s, uint64_t *ns);
[[nodiscard]] char *service_format_duration(uint64_t ns);
[[nodiscard]] char *service_redact_command(const char *raw);
// The replies sent before returning go in p, the command's reply
struct user_packet;
void service_privmsg(struct user *u, const struct user_client *dc, const char *text,
		     struct user_packet *p);
// Global/admin socket entry point. Output and done run on the calling loop;
// data must remain valid through done. Commands can finish asynchronously.
typedef void (*service_print_fn)(void *data, const char *line);
typedef void (*service_done_fn)(void *data);
void service_global(struct server *srv, bool admin, const char *text, service_print_fn print,
		    service_done_fn done, void *data);
