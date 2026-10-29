#include "msgstore/msgstore.h"

#include "base/alloc.h"
#include "base/map.h"

struct ring {
	int64_t network;
	irc_msg_vec messages;
	size_t oldest;
	uint64_t cur;
};

static void ring_free(void *data) {
	struct ring *r = data;
	irc_msg_vec_free(&r->messages);
	xfree(r);
}

void msgstore_init(struct msgstore *s, struct db_thread *db) {
	*s = (struct msgstore){.db = db};
	strmap_init(&s->buffers, ring_free);
}

void msgstore_destroy(struct msgstore *s) {
	strmap_free(&s->buffers);
}

void msgstore_delete_network(struct msgstore *s, int64_t net_id) {
	const char *key;
	void *v;
	for (size_t it = 0; strmap_next(&s->buffers, &it, &key, &v);) {
		struct ring *r = v;
		if (r->network == net_id) {
			strmap_remove(&s->buffers, key);
		}
	}
}

static struct ring *get_ring(struct msgstore *s, int64_t net_id, const char *target, bool create) {
	AUTOFREE char *key = xasprintf("%lld/%s", (long long)net_id, target);
	struct ring *r = strmap_get(&s->buffers, key);
	if (r == nullptr && create) {
		r = xcalloc(1, sizeof(*r));
		r->network = net_id;
		r->cur = 1;
		strmap_set(&s->buffers, key, r);
	}
	return r;
}

struct job {
	int64_t network, last;
	char *target;
	struct irc_msg *message;
	struct db_message_options options;
	char *sender, *text;
	irc_msg_vec messages;
	db_target_last_vec targets;
	struct err *err;
};

static void append_db(struct db *db, void *data) {
	struct job *j = data;
	i64vec ids = {0};
	if (db_store_messages(db, j->network, j->target, &j->message, 1, &ids, &j->err)) {
		j->last = ids.data[0];
	}
	vec_free(&ids);
}

static void last_db(struct db *db, void *data) {
	struct job *j = data;
	bool ok = db_get_message_last_id(db, j->network, j->target, &j->last, &j->err);
	(void)ok;
}

static void query_db(struct db *db, void *data) {
	struct job *j = data;
	bool ok = db_list_messages(db, j->network, j->target, &j->options, &j->messages, &j->err);
	(void)ok;
}

static void targets_db(struct db *db, void *data) {
	struct job *j = data;
	bool ok =
		db_list_message_last_per_target(db, j->network, &j->options, &j->targets, &j->err);
	(void)ok;
}

static bool job_result(struct job *j, struct err **err) {
	xfree(j->target);
	xfree(j->sender);
	xfree(j->text);
	irc_msg_free(j->message);
	if (j->err != nullptr) {
		if (err != nullptr) {
			*err = j->err;
		} else {
			err_free(j->err);
		}
		return false;
	}
	return true;
}

bool msgstore_append(struct msgstore *s, int64_t net_id, const char *target,
		     const struct irc_msg *msg, char **id, struct err **err) {
	*id = nullptr;
	if (s->db != nullptr) {
		struct job j = {.network = net_id,
				.target = xstrdup(target),
				.message = irc_msg_clone(msg)};
		db_thread_call(s->db, append_db, &j);
		if (!job_result(&j, err)) {
			return false;
		}
		*id = format_db_msg_id(net_id, str_from(target), j.last);
		return true;
	}
	if (!cstr_eq(msg->command, "PRIVMSG") && !cstr_eq(msg->command, "NOTICE")) {
		*id = xstrdup("");
		return true;
	}
	struct ring *r = get_ring(s, net_id, target, true);
	if (r->messages.len == MESSAGE_RING_BUFFER_CAP) {
		irc_msg_free(r->messages.data[r->oldest]);
		r->messages.data[r->oldest] = irc_msg_clone(msg);
		r->oldest = (r->oldest + 1) % MESSAGE_RING_BUFFER_CAP;
	} else {
		vec_push(&r->messages, irc_msg_clone(msg));
	}
	*id = format_memory_msg_id(net_id, str_from(target), r->cur++);
	return true;
}

bool msgstore_last_id(struct msgstore *s, int64_t net_id, const char *target, char **id,
		      struct err **err) {
	*id = nullptr;
	if (s->db != nullptr) {
		struct job j = {.network = net_id, .target = xstrdup(target)};
		db_thread_call(s->db, last_db, &j);
		if (!job_result(&j, err)) {
			return false;
		}
		*id = format_db_msg_id(net_id, str_from(target), j.last);
	} else {
		struct ring *r = get_ring(s, net_id, target, false);
		*id = format_memory_msg_id(net_id, str_from(target), r != nullptr ? r->cur - 1 : 0);
	}
	return true;
}

bool msgstore_load_latest_id(struct msgstore *s, int64_t net_id, const char *target, const char *id,
			     size_t limit, bool events, irc_msg_vec *out, struct err **err) {
	if (s->db != nullptr) {
		int64_t after;
		if (!parse_db_msg_id(str_from(id), &after, err)) {
			return false;
		}
		struct db_message_options options = {
			.after_id = after, .limit = limit, .take_last = true, .events = events};
		return msgstore_query(s, net_id, target, &options, out, err);
	}
	if (events) {
		return err_set(err, 0, "events are unsupported for memory message store");
	}
	uint64_t seq;
	if (!parse_memory_msg_id(str_from(id), &seq, err)) {
		return false;
	}
	struct ring *r = get_ring(s, net_id, target, false);
	if (r == nullptr || seq == r->cur) {
		return true;
	}
	if (seq > r->cur) {
		return err_set(
			err, 0,
			"loading messages from sequence number (%llu) greater than the last (%llu)",
			(unsigned long long)seq, (unsigned long long)(r->cur - 1));
	}
	uint64_t available = r->cur - seq - 1;
	size_t n = available < r->messages.len ? (size_t)available : r->messages.len;
	if (n > limit) {
		n = limit;
	}
	for (size_t i = r->messages.len - n; i < r->messages.len; i++) {
		vec_push(out, irc_msg_clone(r->messages.data[(r->oldest + i) % r->messages.len]));
	}
	return true;
}

static void job_options(struct job *j, int64_t net_id, const char *target,
			const struct db_message_options *o) {
	*j = (struct job){.network = net_id,
			  .target = xstrdup(target),
			  .options = *o,
			  .sender = xstrdup(o->sender != nullptr ? o->sender : ""),
			  .text = xstrdup(o->text != nullptr ? o->text : "")};
	j->options.sender = j->sender;
	j->options.text = j->text;
}

bool msgstore_query(struct msgstore *s, int64_t net_id, const char *target,
		    const struct db_message_options *o, irc_msg_vec *out, struct err **err) {
	if (s->db == nullptr) {
		return err_set(err, 0, "chathistory is unsupported for memory message store");
	}
	struct job j;
	job_options(&j, net_id, target, o);
	db_thread_call(s->db, query_db, &j);
	bool ok = job_result(&j, err);
	if (ok) {
		for (size_t i = 0; i < j.messages.len; i++) {
			vec_push(out, j.messages.data[i]);
		}
		vec_free(&j.messages);
	} else {
		irc_msg_vec_free(&j.messages);
	}
	return ok;
}

bool msgstore_targets(struct msgstore *s, int64_t net_id, const struct db_message_options *o,
		      db_target_last_vec *out, struct err **err) {
	if (s->db == nullptr) {
		return err_set(err, 0, "chathistory is unsupported for memory message store");
	}
	struct job j;
	job_options(&j, net_id, "", o);
	db_thread_call(s->db, targets_db, &j);
	bool ok = job_result(&j, err);
	if (ok) {
		for (size_t i = 0; i < j.targets.len; i++) {
			vec_push(out, j.targets.data[i]);
		}
		vec_free(&j.targets);
	} else {
		db_target_last_vec_free(&j.targets);
	}
	return ok;
}
