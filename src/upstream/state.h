// Declarations shared by upstream/channels.c and upstream/users.c, which
// keep the state of channels and users from the server's messages
#pragma once
#include "upstream/internal.h"

#include "base/alloc.h"

static inline void replace(char **p, const char *s) {
	char *v = xstrdup(s);
	xfree(*p);
	*p = v;
}

static inline bool same(struct upstream *uc, const char *a, const char *b) {
	AUTOFREE char *ca = casemap_cstr(uc->casemap, a);
	AUTOFREE char *cb = casemap_cstr(uc->casemap, b);
	return cstr_eq(ca, cb);
}

static inline bool need(struct irc_msg *msg, size_t n) {
	return msg->params.len >= n;
}

void upstream_free_member(void *v);
void upstream_free_user(void *v);
// soju-rs cache_user_info: create is false for messages which only update
// the info of a user we already know
struct upstream_user *upstream_cache_user(struct upstream *uc, const struct irc_prefix *prefix,
					  bool create);
// soju-rs should_cache_user_info
[[nodiscard]] bool upstream_should_cache(struct upstream *uc, const char *nick);
// Forgets a user we don't share a channel with anymore
void upstream_forget_user(struct upstream *uc, const char *nick);
bool upstream_handle_who_reply(struct upstream *uc, struct irc_msg *msg);
bool upstream_handle_monitor(struct upstream *uc, struct irc_msg *msg);
bool upstream_handle_quit_nick(struct upstream *uc, struct irc_msg *msg);
bool upstream_handle_user_update(struct upstream *uc, struct irc_msg *msg);
