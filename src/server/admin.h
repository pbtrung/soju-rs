#pragma once
#include "net/conn.h"
#include "server/server.h"
// Takes the connection reference. Unix admin listeners are owner-only.
void admin_serve(struct server *srv, struct conn *conn);
// Quote command arguments for split_words, rejecting IRC line delimiters.
[[nodiscard]] bool admin_quote_words(const char *const *words, size_t count, struct strbuf *out,
				     struct err **err);

// Offline bootstrap, with all argument checks completed before opening storage.
[[nodiscard]] bool admin_create_turso_admin(const struct config *config, const char *const *words,
					    size_t count, struct err **err);
