// Declarations shared by the files of downstream/, which together port
// soju-rs downstream/: not part of the module's interface (downstream.h)
#pragma once
#include "downstream/downstream.h"

#include "base/alloc.h"
#include "irc/numerics.h"

#define N(a) (sizeof(a) / sizeof((a)[0]))

static inline void replace_str(char **field, const char *value) {
	char *v = xstrdup(value);
	xfree(*field);
	*field = v;
}

// Handler results: an IRC error replied to the client, or a fatal error
struct result {
	struct irc_msg *irc;
	char *fatal;
};

static inline struct result ok(void) {
	return (struct result){0};
}

static inline struct result irc_err(struct irc_msg *m) {
	return (struct result){.irc = m};
}

static inline struct result need_more_params(const char *cmd) {
	return irc_err(IRC_MSG(ERR_NEEDMOREPARAMS, "*", cmd, "Not enough parameters"));
}

static inline bool need_params(const struct irc_msg *m, size_t n) {
	return m->params.len >= n;
}

void downstream_authenticate(struct downstream *dc, const struct downstream_sasl *s);
void downstream_drain_input(struct downstream *dc);
struct result downstream_handle_authenticate(struct downstream *dc, const struct irc_msg *msg,
					     bool *complete);
struct result downstream_handle_cap(struct downstream *dc, const struct irc_msg *msg);
bool downstream_handle_metadata_sub(struct downstream *dc, const struct irc_msg *msg);
struct result downstream_handle_unregistered(struct downstream *dc, const struct irc_msg *msg);
void downstream_label_free(struct label_ctx *l);
void downstream_maybe_register(struct downstream *dc);
void downstream_on_message(struct conn *c, struct irc_msg *msg, void *data);
void downstream_reply_and_close(struct downstream *dc, struct irc_msg *m);
void downstream_resume(struct downstream *dc);
void downstream_set_auth_username(struct downstream *dc, const char *username, const char *client,
				  const char *network);
void downstream_try_register(struct downstream *dc);
