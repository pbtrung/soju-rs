// IRC message parsing and formatting, compatible with soju-rs's irc module
// (itself compatible with gopkg.in/irc.v4).
//
// A struct irc_msg owns everything it points to. Its command, prefix and
// parameters live in a per-message arena (PLAN.md 2.1 rule 7): pointers to
// them are valid until irc_msg_free(), even after the parameter is
// replaced. Copy anything kept longer.
#pragma once

#include <stddef.h>

#include "base/arena.h"
#include "base/str.h"
#include "base/vec.h"

// IRCv3 message tags, in insertion order. Keys and values are owned
// (xmalloc'd) strings; a tag without a value has an empty value.
struct irc_tag {
	char *key;
	char *value;
};

struct irc_tags {
	VEC(struct irc_tag) v;
};

#define IRC_TAGS_INIT ((struct irc_tags){0})

// Parses a tag string (without the leading '@'), appending to tags
void irc_tags_parse(struct irc_tags *tags, struct str s);
// The value, or nullptr if missing
[[nodiscard]] const char *irc_tags_get(const struct irc_tags *tags, const char *key);
// The value, or "" if missing
[[nodiscard]] const char *irc_tags_get_or_empty(const struct irc_tags *tags, const char *key);
[[nodiscard]] bool irc_tags_contains(const struct irc_tags *tags, const char *key);
// Sets a tag; an existing key keeps its position
void irc_tags_set(struct irc_tags *tags, const char *key, const char *value);
void irc_tags_set_str(struct irc_tags *tags, struct str key, struct str value);
// Removes a tag; returns false if it was missing
bool irc_tags_remove(struct irc_tags *tags, const char *key);
// Keeps only the tags for which keep() returns true
void irc_tags_retain(struct irc_tags *tags,
		     bool (*keep)(const char *key, const char *value, void *data), void *data);
void irc_tags_clear(struct irc_tags *tags);
void irc_tags_copy(struct irc_tags *dst, const struct irc_tags *src);
void irc_tags_free(struct irc_tags *tags);
// Appends "k1=v1;k2" (escaped, without '@')
void irc_tags_format(const struct irc_tags *tags, struct strbuf *out);

// Escaping of tag values
void irc_escape_tag_value(struct strbuf *out, struct str v);
void irc_parse_tag_value(struct strbuf *out, struct str v);

// The source of a message; the strings are never nullptr ("" if missing)
struct irc_prefix {
	char *name;
	char *user;
	char *host;
};

// Splits "name!user@host"; the parts are borrowed from s
void irc_prefix_split(struct str s, struct str *name, struct str *user, struct str *host);
// The length of the formatted prefix
[[nodiscard]] size_t irc_prefix_formatted_len(const struct irc_prefix *p);
void irc_prefix_format(const struct irc_prefix *p, struct strbuf *out);

struct irc_msg {
	struct arena arena;
	struct irc_tags tags;
	// nullptr: no prefix. A prefix with an empty name isn't formatted.
	struct irc_prefix *prefix;
	char *command;
	VEC(char *) params;
};

enum irc_parse_error {
	IRC_PARSE_OK,
	IRC_PARSE_ZERO_LENGTH,
	IRC_PARSE_MISSING_DATA_AFTER_PREFIX,
	IRC_PARSE_MISSING_DATA_AFTER_TAGS,
	IRC_PARSE_MISSING_COMMAND,
};

// The error message of soju-rs/gopkg.in/irc.v4 ("irc: ...")
[[nodiscard]] const char *irc_parse_error_str(enum irc_parse_error e);

// Parses a raw IRC line; trailing CR/LF characters are ignored. Returns
// nullptr on error (and sets *err if err isn't nullptr).
[[nodiscard]] struct irc_msg *irc_msg_parse(struct str line, enum irc_parse_error *err);

// Builds a message from a command and a nullptr-terminated parameter list:
//   irc_msg_new_v("PRIVMSG", (const char *[]){"#chan", "hi", nullptr})
// or with the IRC_MSG macro:
//   IRC_MSG("PRIVMSG", "#chan", "hi")
[[nodiscard]] struct irc_msg *irc_msg_new_v(const char *command, const char *const *params);
#define IRC_MSG(command, ...) \
	irc_msg_new_v((command), (const char *const[]){__VA_ARGS__ __VA_OPT__(, ) nullptr})

[[nodiscard]] struct irc_msg *irc_msg_clone(const struct irc_msg *msg);
void irc_msg_free(struct irc_msg *msg);

void irc_msg_set_prefix(struct irc_msg *msg, const char *name, const char *user, const char *host);
void irc_msg_set_prefix_from(struct irc_msg *msg, const struct irc_prefix *prefix);
void irc_msg_clear_prefix(struct irc_msg *msg);
void irc_msg_set_command(struct irc_msg *msg, const char *command);
void irc_msg_add_param(struct irc_msg *msg, const char *param);
void irc_msg_add_param_str(struct irc_msg *msg, struct str param);
// Replaces parameter i (i < params.len)
void irc_msg_set_param(struct irc_msg *msg, size_t i, const char *param);
// Inserts a parameter at index i (i <= params.len)
void irc_msg_insert_param(struct irc_msg *msg, size_t i, const char *param);
void irc_msg_remove_param(struct irc_msg *msg, size_t i);
// Keeps the first n parameters
void irc_msg_truncate_params(struct irc_msg *msg, size_t n);

// The name of the message source, or ""
[[nodiscard]] const char *irc_msg_prefix_name(const struct irc_msg *msg);
// Parameter i, or "" if there's no such parameter
[[nodiscard]] const char *irc_msg_param(const struct irc_msg *msg, size_t i);
// The last parameter, or ""
[[nodiscard]] const char *irc_msg_trailing(const struct irc_msg *msg);

// Appends the formatted message (without CRLF)
void irc_msg_format(const struct irc_msg *msg, struct strbuf *out);
// The formatted message as a new string
[[nodiscard]] char *irc_msg_to_string(const struct irc_msg *msg);

// A list of owned messages
typedef VEC(struct irc_msg *) irc_msg_vec;
void irc_msg_vec_free(irc_msg_vec *v);
