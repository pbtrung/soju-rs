#pragma once
#include "irc/casemapping.h"

struct delivered_store {
	struct cmmap targets;
};
void delivered_init(struct delivered_store *s, enum casemapping cm);
void delivered_destroy(struct delivered_store *s);
// Borrowed ID, or "".
[[nodiscard]] const char *delivered_get(const struct delivered_store *s, const char *target,
					const char *client);
void delivered_set(struct delivered_store *s, const char *target, const char *client,
		   const char *id);
