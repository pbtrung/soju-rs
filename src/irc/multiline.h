// draft/multiline support (soju-rs xirc/multiline.rs).
//
// A multiline message is handled internally as a single logical
// PRIVMSG/NOTICE whose text contains '\n' line separators. It is converted
// from and to a draft/multiline batch at the connection boundaries.
#pragma once

#include <stddef.h>

#include "irc/message.h"

static constexpr char MULTILINE_BATCH_TYPE[] = "draft/multiline";
static constexpr char MULTILINE_CONCAT_TAG[] = "draft/multiline-concat";
// Upper bounds for the size of a multiline message, regardless of the
// limits advertised by the server
static constexpr size_t MAX_MULTILINE_BYTES = (size_t)64 * 1024;
static constexpr size_t MAX_MULTILINE_LINES = 1024;

struct multiline_limits {
	size_t max_bytes;
	size_t max_lines;
};

// soju's own upper bounds
[[nodiscard]] struct multiline_limits multiline_limits_default(void);
// Parses a capability value such as "max-bytes=4096,max-lines=24"; missing
// or invalid limits default to soju's own upper bounds
[[nodiscard]] struct multiline_limits multiline_limits_parse(struct str v);
// "max-bytes=N,max-lines=N"
[[nodiscard]] char *multiline_limits_cap_value(struct multiline_limits l);

// The maximum length of the text of a PRIVMSG/NOTICE line, so that the full
// line fits in an IRC message with a prefix of prefix_len bytes
[[nodiscard]] size_t max_text_length(size_t prefix_len, const char *command, const char *target);

enum text_message_kind {
	TEXT_MESSAGE_SINGLE, // the message as is
	TEXT_MESSAGE_BATCH, // a draft/multiline batch: BATCH +, the lines, BATCH -
	TEXT_MESSAGE_LINES, // one message per line, without draft/multiline
};

// Prepares a message for a peer: a PRIVMSG/NOTICE spanning several lines,
// or too long for a single line, is sent as a draft/multiline batch if the
// peer supports it (multiline), or else line by line. prefix_len and
// batch_ref are only called when needed (batch_ref returns a new string).
// For TEXT_MESSAGE_SINGLE, out is left empty and msg is to be sent as is;
// otherwise out receives the messages to send instead.
[[nodiscard]] enum text_message_kind prepare_text_message(const struct irc_msg *msg, bool multiline,
							  size_t (*prefix_len)(void *data),
							  char *(*batch_ref)(void *data),
							  void *data, irc_msg_vec *out);

struct multiline_line {
	struct str text; // borrowed from the input
	bool concat;
};
typedef VEC(struct multiline_line) multiline_line_vec;

// Splits text into lines of at most max bytes; lines which had to be split
// are continued with lines flagged for concatenation
void split_multiline(multiline_line_vec *out, struct str text, size_t max);
void generate_multiline_batch(irc_msg_vec *out, const struct irc_msg *msg, const char *batch_ref,
			      size_t max);
void generate_multiline_fallback(irc_msg_vec *out, const struct irc_msg *msg, size_t max);

enum multiline_error {
	MULTILINE_OK,
	MULTILINE_MAX_BYTES,
	MULTILINE_MAX_LINES,
	MULTILINE_INVALID_TARGET,
	MULTILINE_INVALID,
};

// The FAIL code: "MULTILINE_MAX_BYTES", ...
[[nodiscard]] const char *multiline_error_code(enum multiline_error e);

// A draft/multiline batch being received
struct multiline_batch {
	struct irc_msg *opener; // the BATCH command which opened the batch
	char *target;
	char *command;
	struct strbuf text;
	size_t bytes, lines;
	enum multiline_error error;
};

// Starts a batch from its "BATCH +<ref> draft/multiline <target>" opener
// (takes ownership)
void multiline_batch_init(struct multiline_batch *b, struct irc_msg *opener);
void multiline_batch_free(struct multiline_batch *b);
// Appends a message of the batch. Once an error has occurred, further
// messages are ignored.
void multiline_batch_push(struct multiline_batch *b, const struct irc_msg *msg,
			  const struct multiline_limits *limits);
// Completes the batch and frees it: returns the logical message (with the
// opener's tags and prefix), or nullptr and sets *error
[[nodiscard]] struct irc_msg *multiline_batch_finish(struct multiline_batch *b,
						     enum multiline_error *error);
