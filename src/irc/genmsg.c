#include "irc/genmsg.h"

#include <stdlib.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/numerics.h"

struct join_pair {
	const char *channel;
	const char *key;
	size_t index;
};

static int cmp_join(const void *pa, const void *pb) {
	const struct join_pair *a = pa, *b = pb;
	bool ha = a->key[0] != '\0', hb = b->key[0] != '\0';
	if (ha != hb) {
		// Only one of the channels has a key
		return ha ? -1 : 1;
	}
	int c = strcmp(a->channel, b->channel);
	if (c != 0) {
		return c;
	}
	// Stable, like sort_by
	return a->index < b->index ? -1 : a->index > b->index;
}

static void flush_join(irc_msg_vec *out, struct strbuf *channels, struct strbuf *keys) {
	struct irc_msg *m = IRC_MSG("JOIN", strbuf_cstr(channels));
	if (keys->len > 0) {
		irc_msg_add_param(m, strbuf_cstr(keys));
	}
	vec_push(out, m);
	strbuf_reset(channels);
	strbuf_reset(keys);
}

void generate_join(irc_msg_vec *out, const char *const *channels, const char *const *keys,
		   size_t n) {
	struct join_pair *pairs = xreallocarray(nullptr, n > 0 ? n : 1, sizeof(*pairs));
	for (size_t i = 0; i < n; i++) {
		pairs[i] = (struct join_pair){.channel = channels[i], .key = keys[i], .index = i};
	}
	qsort(pairs, n, sizeof(*pairs), cmp_join);

	// Two spaces because there are three words (JOIN, channels and keys)
	size_t max_length = MAX_MESSAGE_LENGTH - (strlen("JOIN") + 2);
	AUTO_STRBUF struct strbuf cb = STRBUF_INIT;
	AUTO_STRBUF struct strbuf kb = STRBUF_INIT;
	for (size_t i = 0; i < n; i++) {
		const char *channel = pairs[i].channel, *key = pairs[i].key;
		size_t l = cb.len + kb.len + 1 + strlen(channel);
		if (key[0] != '\0') {
			l += 1 + strlen(key);
		}
		if (cb.len > 0 && l > max_length) {
			// No room for the new channel in this message
			flush_join(out, &cb, &kb);
		}
		if (cb.len > 0) {
			strbuf_append_char(&cb, ',');
		}
		strbuf_append_cstr(&cb, channel);
		if (key[0] != '\0') {
			if (kb.len > 0) {
				strbuf_append_char(&kb, ',');
			}
			strbuf_append_cstr(&kb, key);
		}
	}
	if (cb.len > 0) {
		flush_join(out, &cb, &kb);
	}
	xfree(pairs);
}

static int cmp_cstr(const void *a, const void *b) {
	return strcmp(*(char *const *)a, *(char *const *)b);
}

void generate_isupport(irc_msg_vec *out, const struct isupport *is) {
	strvec tokens = {0};
	const char *k;
	void *v;
	for (size_t it = 0; strmap_next(&is->m, &it, &k, &v);) {
		strvec_push_take(&tokens, v != nullptr ? xasprintf("%s=%s", k, (const char *)v)
						       : xstrdup(k));
	}
	if (tokens.len > 0) {
		qsort(tokens.data, tokens.len, sizeof(*tokens.data), cmp_cstr);
	}

	size_t max_tokens = MAX_MESSAGE_PARAMS - 2; // 2 reserved params: nick + text
	// TODO: take into account MAX_MESSAGE_LENGTH as well
	for (size_t i = 0; i < tokens.len; i += max_tokens) {
		struct irc_msg *m = IRC_MSG(RPL_ISUPPORT, "*");
		for (size_t j = i; j < tokens.len && j < i + max_tokens; j++) {
			AUTO_STRBUF struct strbuf enc = STRBUF_INIT;
			encode_isupport_value(&enc, str_from(tokens.data[j]));
			irc_msg_add_param(m, strbuf_cstr(&enc));
		}
		irc_msg_add_param(m, "are supported");
		vec_push(out, m);
	}
	strvec_free(&tokens);
}

void generate_motd(irc_msg_vec *out, struct str motd) {
	vec_push(out, IRC_MSG(RPL_MOTDSTART, "*", "- Message of the Day -"));
	struct str rest = motd, line;
	while (str_next_field(&rest, '\n', &line)) {
		struct irc_msg *m = IRC_MSG(RPL_MOTD, "*");
		irc_msg_add_param_str(m, line);
		vec_push(out, m);
	}
	vec_push(out, IRC_MSG(RPL_ENDOFMOTD, "*", "End of /MOTD command."));
}

void generate_monitor(irc_msg_vec *out, const char *subcmd, const char *const *targets, size_t n) {
	size_t max_length = MAX_MESSAGE_LENGTH - (strlen("MONITOR ") + strlen(subcmd) + 1);
	AUTO_STRBUF struct strbuf buf = STRBUF_INIT;
	size_t count = 0, l = 0;
	for (size_t i = 0; i < n; i++) {
		size_t tl = strlen(targets[i]);
		if (count > 0 && l + tl + 1 > max_length) {
			vec_push(out, IRC_MSG("MONITOR", subcmd, strbuf_cstr(&buf)));
			strbuf_reset(&buf);
			count = 0;
			l = 0;
		}
		if (count > 0) {
			strbuf_append_char(&buf, ',');
		}
		strbuf_append_cstr(&buf, targets[i]);
		count++;
		l += tl + 1;
	}
	if (count > 0) {
		vec_push(out, IRC_MSG("MONITOR", subcmd, strbuf_cstr(&buf)));
	}
}

void generate_names_reply(irc_msg_vec *out, const char *channel, enum channel_status status,
			  const char *const *members, size_t n) {
	const char *st = channel_status_str(status);
	struct irc_msg *empty = IRC_MSG(RPL_NAMREPLY, "*", st, channel, "");
	AUTOFREE char *empty_s = irc_msg_to_string(empty);
	irc_msg_free(empty);
	size_t el = strlen(empty_s);
	size_t max_length = MAX_MESSAGE_LENGTH > el ? MAX_MESSAGE_LENGTH - el : 0;

	AUTO_STRBUF struct strbuf buf = STRBUF_INIT;
	for (size_t i = 0; i < n; i++) {
		size_t l = buf.len + 1 + strlen(members[i]);
		if (buf.len > 0 && l > max_length) {
			// There's not enough space for the next space + nick
			vec_push(out, IRC_MSG(RPL_NAMREPLY, "*", st, channel, strbuf_cstr(&buf)));
			strbuf_reset(&buf);
		}
		if (buf.len > 0) {
			strbuf_append_char(&buf, ' ');
		}
		strbuf_append_cstr(&buf, members[i]);
	}
	if (buf.len > 0) {
		vec_push(out, IRC_MSG(RPL_NAMREPLY, "*", st, channel, strbuf_cstr(&buf)));
	}
	vec_push(out, IRC_MSG(RPL_ENDOFNAMES, "*", channel, "End of /NAMES list"));
}

void generate_sasl(irc_msg_vec *out, const void *resp, size_t len) {
	AUTOFREE char *enc = base64_encode_cstr(BASE64_STANDARD, resp, len);
	size_t el = strlen(enc);
	// <= instead of < because we need to send a final empty response if
	// the last chunk is exactly 400 bytes long
	for (size_t i = 0; i <= el; i += MAX_SASL_LENGTH) {
		size_t j = i + MAX_SASL_LENGTH < el ? i + MAX_SASL_LENGTH : el;
		struct irc_msg *m = IRC_MSG("AUTHENTICATE");
		if (j == i) {
			irc_msg_add_param(m, "+");
		} else {
			irc_msg_add_param_str(m, str_new(enc + i, j - i));
		}
		vec_push(out, m);
	}
}
