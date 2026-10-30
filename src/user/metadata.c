// Metadata is persisted on the DB thread; the user actor owns subscriptions.
#include "user/internal.h"
#include "base/alloc.h"
#include "base/utf8.h"

static const char *const keys[] = {"soju.im/pinned", "soju.im/muted", "soju.im/blocked"};
static int key_index(const char *key) {
	for (int i = 0; i < 3; i++) {
		if (cstr_eq(key, keys[i])) {
			return i;
		}
	}
	return -1;
}
static bool *field(struct db_message_target *mt, int i) {
	return i == 0 ? &mt->pinned : i == 1 ? &mt->muted : &mt->blocked;
}
struct metadata_job {
	struct server *srv;
	int64_t user, network;
	uint64_t client;
	struct user_packet *reply;
	struct irc_msg *command;
	char *target;
	struct db_message_target mt;
	db_message_target_vec targets;
	struct err *err;
	bool has_target, changed[3], subscribe, unsubscribe, list_subs, initial;
};
static void metadata_free(struct metadata_job *j) {
	irc_msg_free(j->command);
	db_message_target_free(&j->mt);
	db_message_target_vec_free(&j->targets);
	err_free(j->err);
	xfree(j->target);
	xfree(j);
}
static void metadata_drop(void *data) {
	struct metadata_job *j = data;
	user_packet_send_take(j->reply); // release the source input slot after deletion
	metadata_free(j);
}
static char *lower(const char *text) {
	struct strbuf result = {0};
	utf8_to_lower(&result, str_from(text));
	return strbuf_take(&result);
}
static struct irc_msg *invalid_key(const char *key) {
	return IRC_MSG("FAIL", "METADATA", "KEY_INVALID", key, "Invalid key");
}
static void run_metadata(struct db *db, void *data) {
	struct metadata_job *j = data;
	if (j->subscribe || j->initial) {
		if (j->network) {
			bool ok = db_list_message_targets(db, j->network, &j->targets, &j->err);
			(void)ok;
		}
		return;
	}
	if (j->unsubscribe || j->list_subs) {
		return;
	}
	if (j->has_target && !db_get_message_target(db, j->network, j->target, &j->mt, &j->err)) {
		return;
	}
	struct user_packet *p = j->reply;
	const struct irc_msg *msg = j->command;
	const char *target = irc_msg_param(msg, 0), *sub = irc_msg_param(msg, 1);
	if (cstr_eq(sub, "GET") || cstr_eq(sub, "LIST")) {
		p->batch_type = xstrdup("metadata");
		if (cstr_eq(sub, "LIST")) {
			for (int i = 0; j->has_target && i < 3; i++) {
				user_packet_add_take(p, IRC_MSG("761", "*", target, keys[i], "*",
								*field(&j->mt, i) ? "1" : "0"));
			}
		} else {
			for (size_t i = 2; i < msg->params.len; i++) {
				AUTOFREE char *key = lower(irc_msg_param(msg, i));
				int index = key_index(key);
				user_packet_add_take(
					p, j->has_target && index >= 0
						   ? IRC_MSG("761", "*", target, key, "*",
							     *field(&j->mt, index) ? "1" : "0")
						   : invalid_key(key));
			}
		}
	} else if (cstr_eq(sub, "SET") || cstr_eq(sub, "CLEAR")) {
		bool clear = cstr_eq(sub, "CLEAR");
		if (clear) {
			p->batch_type = xstrdup("metadata");
			p->metadata_updates_unbatched = true;
		} else if (msg->params.len < 3) {
			user_packet_add_take(
				p, IRC_MSG("461", "*", "METADATA", "Not enough parameters"));
			return;
		}
		for (int i = 0; i < (clear ? 3 : 1); i++) {
			AUTOFREE char *key = lower(clear ? keys[i] : irc_msg_param(msg, 2));
			int index = key_index(key);
			const char *value =
				!clear && msg->params.len > 3 ? irc_msg_param(msg, 3) : "0";
			if (!j->has_target || index < 0) {
				user_packet_add_take(p, invalid_key(key));
			} else if (!cstr_eq(value, "0") && !cstr_eq(value, "1")) {
				user_packet_add_take(p, IRC_MSG("FAIL", "METADATA", "VALUE_INVALID",
								"Invalid value"));
			} else {
				*field(&j->mt, index) = cstr_eq(value, "1");
				j->changed[index] = true;
				user_packet_add_take(p,
						     IRC_MSG("761", "*", target, key, "*", value));
			}
		}
		if (j->has_target) {
			bool ok = db_store_message_target(db, j->network, &j->mt, &j->err);
			(void)ok;
		}
	} else {
		struct irc_msg *error =
			IRC_MSG("FAIL", "METADATA", "INVALID_PARAMS", sub, "Unknown command");
		server_set_prefix(j->srv, error);
		user_packet_add_take(p, error);
	}
}
static void apply_metadata(struct user *u, void *data) {
	struct metadata_job *j = data;
	struct user_client *dc = user_get_client(u, j->client);
	struct user_packet *p = j->reply;
	if (j->initial) {
		if (dc != nullptr && j->err == nullptr) {
			for (size_t i = 0; i < j->targets.len; i++) {
				struct db_message_target *mt = &j->targets.data[i];
				irc_msg_vec messages = {0};
				for (int k = 0; k < 3; k++) {
					if (strmap_has(&dc->metadata_subs, keys[k])) {
						vec_push(&messages,
							 IRC_MSG("METADATA", mt->target, keys[k],
								 "*", *field(mt, k) ? "1" : "0"));
					}
				}
				vec_push(&p->target_metadata, messages);
			}
		}
		user_packet_send_take(p);
		metadata_free(j);
		return;
	}
	if (j->err != nullptr) {
		irc_msg_vec_free(&p->messages);
		struct irc_msg *error = IRC_MSG("FAIL", "METADATA", "INTERNAL_ERROR",
						irc_msg_param(j->command, 0), "Internal error");
		server_set_prefix(j->srv, error);
		user_packet_add_take(p, error);
	} else if (dc != nullptr && (j->subscribe || j->unsubscribe)) {
		for (size_t i = 2; i < j->command->params.len; i++) {
			AUTOFREE char *key = lower(irc_msg_param(j->command, i));
			int index = key_index(key);
			if (index < 0) {
				user_packet_add_take(p, invalid_key(key));
			} else if (j->subscribe) {
				strmap_set(&dc->metadata_subs, key, nullptr);
				for (size_t k = 0; k < j->targets.len; k++) {
					struct db_message_target *mt = &j->targets.data[k];
					user_packet_add_take(
						p, IRC_MSG("METADATA", mt->target, key, "*",
							   *field(mt, index) ? "1" : "0"));
				}
				user_packet_add_take(p, IRC_MSG("770", "*", key));
			} else {
				strmap_remove(&dc->metadata_subs, key);
				user_packet_add_take(p, IRC_MSG("771", "*", key));
			}
		}
	} else if (dc != nullptr && j->list_subs) {
		const char *key;
		for (size_t it = 0; strmap_next(&dc->metadata_subs, &it, &key, nullptr);) {
			user_packet_add_take(p, IRC_MSG("772", "*", key));
		}
	}
	struct user_network *net = user_get_network(u, j->network);
	if (net != nullptr && j->changed[2] && j->err == nullptr) {
		if (j->mt.blocked) {
			cmmap_set(&net->blocked_targets, j->target, nullptr);
		} else {
			cmmap_del(&net->blocked_targets, j->target);
		}
	}
	irc_msg_vec messages = p->messages;
	p->messages = (irc_msg_vec){0};
	for (size_t i = 0; i < messages.len; i++) {
		struct irc_msg *message = messages.data[i];
		user_packet_add_take(p, message);
		int k = key_index(irc_msg_param(message, 2));
		if (cstr_eq(message->command, "761") && k >= 0 && j->changed[k] &&
		    j->err == nullptr) {
			user_packet_add_take(p,
					     IRC_MSG("METADATA", irc_msg_param(j->command, 0),
						     keys[k], "*", *field(&j->mt, k) ? "1" : "0"));
		}
	}
	vec_free(&messages);
	// State changes reach every device on this network, including the source.
	for (int k = 0; k < 3; k++) {
		if (!j->changed[k] || j->err != nullptr) {
			continue;
		}
		for (size_t i = 0; i < u->clients.len; i++) {
			struct user_client *peer = u->clients.data[i];
			if (peer->network_id == j->network) {
				struct irc_msg *update =
					IRC_MSG("METADATA", irc_msg_param(j->command, 0), keys[k],
						"*", *field(&j->mt, k) ? "1" : "0");
				if (peer->id == j->client) {
					irc_msg_free(update);
				} else {
					user_send_take(u, peer, update);
				}
			}
		}
	}
	user_packet_send_take(p);
	metadata_free(j);
}
static void metadata_done(void *data) {
	struct metadata_job *j = data;
	user_post(j->srv, j->user, apply_metadata, metadata_drop, j);
}
void user_metadata(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		   struct user_packet *p) {
	if (msg->params.len < 2) {
		user_packet_add_take(p, IRC_MSG("461", "*", "METADATA", "Not enough parameters"));
		user_packet_send_take(p);
		return;
	}
	const char *target = irc_msg_param(msg, 0), *sub = irc_msg_param(msg, 1);
	if (target[0] == '\0' || target[0] == ':' || strchr(target, ' ') != nullptr) {
		struct irc_msg *error =
			IRC_MSG("FAIL", "METADATA", "INVALID_TARGET", "*", "Invalid target");
		server_set_prefix(u->srv, error);
		user_packet_add_take(p, error);
		user_packet_send_take(p);
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	struct metadata_job *j = xcalloc(1, sizeof(*j));
	*j = (struct metadata_job){.srv = u->srv,
				   .user = u->record.id,
				   .network = dc->network_id,
				   .client = dc->id,
				   .reply = p,
				   .command = irc_msg_clone(msg),
				   .target = casemap_cstr(net != nullptr && net->upstream != nullptr
								  ? net->upstream->casemap
								  : CASEMAPPING_RFC1459,
							  target),
				   .has_target = net != nullptr && !cstr_eq(target, "*"),
				   .subscribe = cstr_eq(sub, "SUB"),
				   .unsubscribe = cstr_eq(sub, "UNSUB"),
				   .list_subs = cstr_eq(sub, "SUBS")};
	server_db(u->srv, run_metadata, metadata_done, j);
}

void user_initial_metadata(struct user *u, const struct user_client *dc, struct user_packet *p) {
	struct metadata_job *j = xcalloc(1, sizeof(*j));
	*j = (struct metadata_job){.srv = u->srv,
				   .user = u->record.id,
				   .network = dc->network_id,
				   .client = dc->id,
				   .reply = p,
				   .initial = true};
	server_db(u->srv, run_metadata, metadata_done, j);
}
