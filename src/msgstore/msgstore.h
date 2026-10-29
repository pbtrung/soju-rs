// User-owned message stores. Database jobs copy their inputs and return
// fresh results. Local SQLite calls wait for the DB thread, matching the
// synchronous local-storage sections of the Rust user actor.
#pragma once
#include "base/map.h"
#include "db/dbthread.h"
#include "msgstore/msgid.h"

static constexpr size_t MESSAGE_RING_BUFFER_CAP = 4096;
struct msgstore {
	struct strmap buffers;
	struct db_thread *db; // nullptr: memory
};
void msgstore_init(struct msgstore *s, struct db_thread *db);
void msgstore_destroy(struct msgstore *s);
void msgstore_delete_network(struct msgstore *s, int64_t net_id);
[[nodiscard]] bool msgstore_append(struct msgstore *s, int64_t net_id, const char *target,
				   const struct irc_msg *msg, char **id, struct err **err);
[[nodiscard]] bool msgstore_last_id(struct msgstore *s, int64_t net_id, const char *target,
				    char **id, struct err **err);
[[nodiscard]] bool msgstore_load_latest_id(struct msgstore *s, int64_t net_id, const char *target,
					   const char *id, size_t limit, bool events,
					   irc_msg_vec *out, struct err **err);
[[nodiscard]] bool msgstore_query(struct msgstore *s, int64_t net_id, const char *target,
				  const struct db_message_options *options, irc_msg_vec *out,
				  struct err **err);
[[nodiscard]] bool msgstore_targets(struct msgstore *s, int64_t net_id,
				    const struct db_message_options *options,
				    db_target_last_vec *out, struct err **err);
