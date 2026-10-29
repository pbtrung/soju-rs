#include "msgstore/msgstore.h"

#include "base/alloc.h"
#include "base/map.h"
#include "msgstore/turso.h"

#include <stdlib.h>
#include <string.h>

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

static void poll_batches(uv_timer_t *t) {
	struct msgstore *s = t->data;
	struct err *e = nullptr;
	if (!batch_store_poll(s->batch, &e)) {
		log_info(&s->log, "Turso message batch failed; retaining it for retry: %s",
			 err_msg(e));
		err_free(e);
	}
}

void msgstore_start_batches(struct msgstore *s, uv_loop_t *loop, const struct logger *log,
			    int64_t user_id, size_t limit) {
	if (s->db == nullptr || !db_is_remote(db_thread_db(s->db))) {
		return;
	}
	log_with_prefix(&s->log, log, "");
	s->batch = batch_store_new(db_thread_db(s->db), user_id, limit);
	struct err *e = nullptr;
	if (!batch_store_prepare(s->batch, &e)) {
		log_info(&s->log, "failed to prepare Turso message buffers: %s", err_msg(e));
		err_free(e);
	}
	s->timer = xcalloc(1, sizeof(*s->timer));
	uv_timer_init(loop, s->timer);
	s->timer->data = s;
	uv_timer_start(s->timer, poll_batches, 100, 100);
}

static void timer_closed(uv_handle_t *h) {
	xfree(h);
}

void msgstore_destroy(struct msgstore *s) {
	strmap_free(&s->buffers);
	if (s->batch == nullptr) {
		return;
	}
	uv_timer_stop(s->timer);
	uv_close((uv_handle_t *)s->timer, timer_closed);
	struct err *e = nullptr;
	if (!batch_store_flush(s->batch, &e)) {
		log_info(&s->log, "%s", err_msg(e));
		err_free(e);
	}
	batch_store_free(s->batch);
	log_free(&s->log);
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
	db_message_row_vec rows;
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

static void rows_db(struct db *db, void *data) {
	struct job *j = data;
	bool ok = db_list_message_rows(db, j->network, j->target, &j->options, &j->rows, &j->err);
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
	if (s->batch != nullptr) {
		int64_t n = 0;
		if (!batch_store_append(s->batch, net_id, target, msg, &n, err)) {
			return false;
		}
		*id = format_db_msg_id(net_id, str_from(target), n);
		return true;
	}
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
		int64_t pending = 0;
		if (s->batch != nullptr &&
		    (!db_get_message_last_id(batch_store_index(s->batch), net_id, target, &pending,
					     err))) {
			return false;
		}
		j.last = pending > j.last ? pending : j.last;
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

static int row_cmp(const void *a, const void *b) {
	const struct db_message_row *x = a, *y = b;
	int c = strcmp(x->time, y->time);
	return c != 0 ? c : (x->id > y->id) - (x->id < y->id);
}

// Committed and buffered lines, in time and ID order (soju-rs DbStore::list)
static bool query_batches(struct msgstore *s, int64_t net_id, const char *target,
			  const struct db_message_options *o, irc_msg_vec *out, struct err **err) {
	// The overlay is read BEFORE the database: a write completing meanwhile
	// can't make a line missing, only duplicated
	db_message_row_vec rows = {0};
	if (!db_list_message_rows(batch_store_index(s->batch), net_id, target, o, &rows, err)) {
		return false;
	}
	struct job j;
	job_options(&j, net_id, target, o);
	db_thread_call(s->db, rows_db, &j);
	if (!job_result(&j, err)) {
		db_message_row_vec_free(&rows);
		db_message_row_vec_free(&j.rows);
		return false;
	}
	for (size_t i = 0; i < j.rows.len; i++) {
		vec_push(&rows, j.rows.data[i]);
	}
	vec_free(&j.rows);
	if (rows.len > 1) {
		qsort(rows.data, rows.len, sizeof(rows.data[0]), row_cmp);
	}
	db_message_row_vec kept = {0};
	for (size_t i = 0; i < rows.len; i++) {
		bool dup = false;
		for (size_t k = 0; k < kept.len && !dup; k++) {
			dup = kept.data[k].id == rows.data[i].id;
		}
		if (dup) {
			irc_msg_free(rows.data[i].message);
			xfree(rows.data[i].time);
		} else {
			vec_push(&kept, rows.data[i]);
		}
	}
	vec_free(&rows);
	size_t start = o->take_last && kept.len > o->limit ? kept.len - o->limit : 0;
	for (size_t i = 0; i < kept.len; i++) {
		if (i >= start && i - start < o->limit) {
			vec_push(out, kept.data[i].message);
		} else {
			irc_msg_free(kept.data[i].message);
		}
		xfree(kept.data[i].time);
	}
	vec_free(&kept);
	return true;
}

bool msgstore_query(struct msgstore *s, int64_t net_id, const char *target,
		    const struct db_message_options *o, irc_msg_vec *out, struct err **err) {
	if (s->db == nullptr) {
		return err_set(err, 0, "chathistory is unsupported for memory message store");
	}
	if (s->batch != nullptr) {
		return query_batches(s, net_id, target, o, out, err);
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

static int target_cmp(const void *a, const void *b) {
	const struct db_target_last *x = a, *y = b;
	int c = timestamp_cmp(x->latest_message.t, y->latest_message.t);
	return c != 0 ? c : strcmp(x->name, y->name);
}

// Bounds apply to each target's latest line AFTER combining buffered and
// committed lines, out-of-order server times included
static bool targets_batches(struct msgstore *s, int64_t net_id,
			    const struct db_message_options *o, db_target_last_vec *out,
			    struct err **err) {
	struct db_message_options all = {.limit = INT64_MAX, .events = o->events};
	db_target_last_vec v = {0};
	if (!db_list_message_last_per_target(batch_store_index(s->batch), net_id, &all, &v, err)) {
		return false;
	}
	struct job j;
	job_options(&j, net_id, "", &all);
	db_thread_call(s->db, targets_db, &j);
	if (!job_result(&j, err)) {
		db_target_last_vec_free(&v);
		db_target_last_vec_free(&j.targets);
		return false;
	}
	for (size_t i = 0; i < j.targets.len; i++) {
		vec_push(&v, j.targets.data[i]);
	}
	vec_free(&j.targets);
	db_target_last_vec merged = {0};
	for (size_t i = 0; i < v.len; i++) {
		struct db_target_last *t = &v.data[i], *m = nullptr;
		for (size_t k = 0; k < merged.len && m == nullptr; k++) {
			m = cstr_eq(merged.data[k].name, t->name) ? &merged.data[k] : nullptr;
		}
		if (m == nullptr) {
			vec_push(&merged, *t);
			continue;
		}
		if (t->latest_message.set &&
		    (!m->latest_message.set ||
		     timestamp_cmp(t->latest_message.t, m->latest_message.t) > 0)) {
			m->latest_message = t->latest_message;
		}
		xfree(t->name);
	}
	vec_free(&v);
	db_target_last_vec kept = {0};
	for (size_t i = 0; i < merged.len; i++) {
		struct db_target_last *t = &merged.data[i];
		bool drop = !t->latest_message.set ||
			    (o->after_time.set &&
			     timestamp_cmp(t->latest_message.t, o->after_time.t) <= 0) ||
			    (o->before_time.set &&
			     timestamp_cmp(t->latest_message.t, o->before_time.t) >= 0);
		if (drop) {
			xfree(t->name);
		} else {
			vec_push(&kept, *t);
		}
	}
	vec_free(&merged);
	if (kept.len > 1) {
		qsort(kept.data, kept.len, sizeof(kept.data[0]), target_cmp);
	}
	size_t start = o->take_last && kept.len > o->limit ? kept.len - o->limit : 0;
	for (size_t i = 0; i < kept.len; i++) {
		if (i >= start && i - start < o->limit) {
			vec_push(out, kept.data[i]);
		} else {
			xfree(kept.data[i].name);
		}
	}
	vec_free(&kept);
	return true;
}

bool msgstore_targets(struct msgstore *s, int64_t net_id, const struct db_message_options *o,
		      db_target_last_vec *out, struct err **err) {
	if (s->db == nullptr) {
		return err_set(err, 0, "chathistory is unsupported for memory message store");
	}
	if (s->batch != nullptr) {
		return targets_batches(s, net_id, o, out, err);
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
