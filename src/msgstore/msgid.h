// Internal message IDs: a BARE header (version, network ID, target, type)
// followed by a type-specific body, base64url-encoded, compatible with
// upstream (soju-rs msgstore/mod.rs, memory.rs, db.rs)
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/str.h"

enum {
	MSG_ID_NONE = 0,
	MSG_ID_MEMORY = 1,
	// Used by upstream's fs store, which isn't supported: such IDs may still
	// be found in databases, e.g. in delivery receipts
	MSG_ID_FS = 2,
	MSG_ID_DB = 3,
};

// Parses the header of an ID. If typ isn't MSG_ID_NONE, checks the type.
// The target (which may contain NUL bytes) and the rest of the decoded ID
// are appended to target and body (either may be nullptr).
[[nodiscard]] bool parse_msg_id_with(struct str s, uint64_t typ, int64_t *net_id,
				     struct strbuf *target, struct strbuf *body, struct err **err);
// The network ID and target of an ID
[[nodiscard]] bool parse_msg_id(struct str s, int64_t *net_id, struct strbuf *target,
				struct err **err);

[[nodiscard]] char *format_memory_msg_id(int64_t net_id, struct str entity, uint64_t seq);
[[nodiscard]] bool parse_memory_msg_id(struct str s, uint64_t *seq, struct err **err);
[[nodiscard]] char *format_db_msg_id(int64_t net_id, struct str target, int64_t msg_id);
[[nodiscard]] bool parse_db_msg_id(struct str s, int64_t *msg_id, struct err **err);
