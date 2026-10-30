// Mentions recorded for soju.im/unread (soju-rs upstream::record_mention):
// the latest time of each conversation mentioning the user, coalesced for
// MENTION_FLUSH_MS, then stored by one DB job
#include "user/internal.h"

#include "base/alloc.h"

// How long mentions are coalesced before they're stored: a flood of
// private messages costs one write per conversation and period
static constexpr uint64_t MENTION_FLUSH_MS = 1000;

struct mention {
	int64_t net_id;
	char *target;
	struct timestamp time;
};
typedef VEC(struct mention) mention_vec;

// The mentions of a period, owned by its timer: a user may be freed before
// the timer is closed
struct mention_batch {
	uv_timer_t timer;
	struct server *srv;
	int64_t user_id;
	mention_vec pending;
};

struct mention_job {
	struct server *srv;
	mention_vec mentions;
	struct err *err;
};

static void mentions_free(mention_vec *v) {
	for (size_t i = 0; i < v->len; i++) {
		xfree(v->data[i].target);
	}
	vec_free(v);
}

static void job_run(struct db *db, void *data) {
	struct mention_job *j = data;
	for (size_t i = 0; i < j->mentions.len && j->err == nullptr; i++) {
		struct mention *m = &j->mentions.data[i];
		bool ok = db_store_message_target_highlight(db, m->net_id, m->target, m->time,
							    &j->err);
		(void)ok;
	}
}

static void job_done(void *data) {
	struct mention_job *j = data;
	if (j->err != nullptr) {
		log_info(&j->srv->log, "failed to record mentions: %s", err_msg(j->err));
		err_free(j->err);
	}
	mentions_free(&j->mentions);
	xfree(j);
}

static void batch_closed(uv_handle_t *h) {
	struct mention_batch *b = h->data;
	atomic_fetch_sub(&b->srv->pending_work, 1);
	mentions_free(&b->pending);
	xfree(b);
}

// Stores the batch's mentions and closes it
static void batch_flush(struct mention_batch *b) {
	if (b->pending.len > 0) {
		struct mention_job *j = xcalloc(1, sizeof(*j));
		*j = (struct mention_job){.srv = b->srv, .mentions = b->pending};
		b->pending = (mention_vec){0};
		server_db(b->srv, job_run, job_done, j);
	}
	uv_timer_stop(&b->timer);
	uv_close((uv_handle_t *)&b->timer, batch_closed);
}

static void batch_fired(uv_timer_t *t) {
	struct mention_batch *b = t->data;
	struct user *u = user_lookup(b->srv, b->user_id);
	if (u != nullptr && u->mentions == b) {
		u->mentions = nullptr;
	}
	batch_flush(b);
}

void user_record_mention(struct user *u, int64_t net_id, const char *target_cm,
			 struct timestamp time) {
	struct mention_batch *b = u->mentions;
	if (b == nullptr) {
		b = xcalloc(1, sizeof(*b));
		*b = (struct mention_batch){.srv = u->srv, .user_id = u->record.id};
		atomic_fetch_add(&u->srv->pending_work, 1);
		uv_timer_init(&u->loop->uv, &b->timer);
		b->timer.data = b;
		uv_timer_start(&b->timer, batch_fired, MENTION_FLUSH_MS, 0);
		u->mentions = b;
	}
	for (size_t i = 0; i < b->pending.len; i++) {
		struct mention *m = &b->pending.data[i];
		if (m->net_id == net_id && cstr_eq(m->target, target_cm)) {
			if (timestamp_cmp(time, m->time) > 0) {
				m->time = time;
			}
			return;
		}
	}
	vec_push(&b->pending, ((struct mention){net_id, xstrdup(target_cm), time}));
}

void user_mentions_stop(struct user *u) {
	if (u->mentions != nullptr) {
		// Stored now rather than lost
		batch_flush(u->mentions);
		u->mentions = nullptr;
	}
}
