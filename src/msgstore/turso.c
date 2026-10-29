#include "msgstore/turso.h"

#include <pthread.h>
#include <stdatomic.h>
#include <uv.h>

#include "base/alloc.h"
#include "config/config.h"

static constexpr uint64_t FLUSH_INTERVAL_MS = 1000;
static constexpr uint64_t RESERVE_RETRY_MS = 5000;
// A buffer also leaves once it holds this many bytes
static constexpr size_t MAX_BUFFER_BYTES = (size_t)8 << 20U;

typedef VEC(struct db_message_insert) insert_vec;

// A batch being written by a thread
struct flight {
	insert_vec rows;
	pthread_t thread;
	bool running;
	atomic_bool done;
	bool ok;
	size_t rejected;
	struct err *err;
	struct db_message_writer *writer;
	uint64_t retry_at;
	unsigned failures;
};

// IDs being reserved by a thread
struct reservation {
	pthread_t thread;
	bool running;
	atomic_bool done;
	bool ok;
	int64_t count, first, last;
	struct err *err;
	struct db_message_writer *writer;
};

struct batch_store {
	struct db *db;
	int64_t user_id;
	size_t limit;
	bool prepared;
	struct db_message_writer *writer, *allocator;
	struct db *index;
	int64_t next_id, end_id; // available IDs: next_id..end_id
	bool has_next;
	int64_t next_first, next_last;
	struct reservation res;
	uint64_t reserve_retry;
	insert_vec current;
	size_t current_bytes;
	uint64_t since;
	struct flight *flight;
};

static uint64_t now_ms(void) {
	return uv_hrtime() / 1000000U;
}

static void free_rows(insert_vec *v) {
	for (size_t i = 0; i < v->len; i++) {
		db_message_insert_free(&v->data[i]);
	}
	vec_free(v);
}

struct batch_store *batch_store_new(struct db *db, int64_t user_id, size_t limit) {
	struct batch_store *b = xcalloc(1, sizeof(*b));
	b->db = db;
	b->user_id = user_id;
	b->limit = limit < 1 ? 1 : limit > MAX_CHAT_HISTORY_LIMIT ? MAX_CHAT_HISTORY_LIMIT : limit;
	b->next_id = 1;
	return b;
}

static void flight_free(struct flight *f) {
	if (f == nullptr) {
		return;
	}
	if (f->running) {
		pthread_join(f->thread, nullptr);
	}
	free_rows(&f->rows);
	err_free(f->err);
	xfree(f);
}

void batch_store_free(struct batch_store *b) {
	if (b == nullptr) {
		return;
	}
	flight_free(b->flight);
	if (b->res.running) {
		pthread_join(b->res.thread, nullptr);
	}
	err_free(b->res.err);
	free_rows(&b->current);
	db_message_writer_free(b->writer);
	db_message_writer_free(b->allocator);
	if (b->index != nullptr) {
		db_close(b->index);
	}
	xfree(b);
}

struct db *batch_store_index(struct batch_store *b) {
	return b->index;
}

static void *reserve_run(void *arg) {
	struct reservation *r = arg;
	r->ok = db_message_writer_reserve(r->writer, r->count, &r->first, &r->last, &r->err);
	atomic_store(&r->done, true);
	return nullptr;
}

// Reservations are prefetched by a thread, never behind a commit or a
// synchronous HTTP call of the user's loop
// Takes the result of a finished reservation (joins its thread)
static bool collect(struct batch_store *b, struct err **err) {
	struct reservation *r = &b->res;
	pthread_join(r->thread, nullptr);
	r->running = false;
	b->reserve_retry = now_ms() + RESERVE_RETRY_MS;
	if (!r->ok) {
		if (err != nullptr) {
			*err = r->err;
		} else {
			err_free(r->err);
		}
		r->err = nullptr;
		return false;
	}
	b->has_next = true;
	b->next_first = r->first;
	b->next_last = r->last;
	return true;
}

static bool reserve_ahead(struct batch_store *b, struct err **err) {
	struct reservation *r = &b->res;
	if (r->running && atomic_load(&r->done) && !collect(b, err)) {
		return false;
	}
	if (!b->has_next && !r->running && now_ms() >= b->reserve_retry) {
		*r = (struct reservation){.writer = b->allocator,
					  .count = (int64_t)b->limit * 1024};
		atomic_init(&r->done, false);
		if (pthread_create(&r->thread, nullptr, reserve_run, r) != 0) {
			return err_set(err, 0, "failed to start the Turso message ID allocator");
		}
		r->running = true;
	}
	return true;
}

bool batch_store_prepare(struct batch_store *b, struct err **err) {
	if (b->prepared) {
		return true;
	}
	b->writer = db_message_writer_new(b->db, b->user_id, err);
	b->allocator =
		b->writer != nullptr ? db_message_writer_new(b->db, b->user_id, err) : nullptr;
	// Amortized reservations. IDs are never reused across crashes: gaps in
	// history cursors are intentional and harmless.
	if (b->allocator == nullptr ||
	    !db_message_writer_reserve(b->writer, (int64_t)b->limit * 1024, &b->next_id, &b->end_id,
				       err) ||
	    !db_message_buffer_open(&b->index, err)) {
		return false;
	}
	b->prepared = true;
	b->since = now_ms();
	return reserve_ahead(b, err);
}

static void *flight_run(void *arg) {
	struct flight *f = arg;
	f->ok = db_message_writer_write(f->writer, f->rows.data, f->rows.len, &f->rejected,
					&f->err);
	atomic_store(&f->done, true);
	return nullptr;
}

static bool start(struct flight *f, struct err **err) {
	atomic_init(&f->done, false);
	err_free(f->err);
	f->err = nullptr;
	if (pthread_create(&f->thread, nullptr, flight_run, f) != 0) {
		return err_set(err, 0, "failed to start the Turso message writer");
	}
	f->running = true;
	return true;
}

// Collects the in-flight batch once written (waiting if asked), or
// restarts a failed one when due
static bool reap(struct batch_store *b, bool wait, struct err **err) {
	struct flight *f = b->flight;
	if (f == nullptr) {
		return true;
	}
	if (!f->running) {
		return (now_ms() < f->retry_at && !wait) || start(f, err);
	}
	if (!wait && !atomic_load(&f->done)) {
		return true;
	}
	pthread_join(f->thread, nullptr);
	f->running = false;
	if (f->ok) {
		size_t rejected = f->rejected;
		bool ok = db_message_buffer_remove(b->index, f->rows.data, f->rows.len, err);
		if (!ok) {
			return false;
		}
		flight_free(f);
		b->flight = nullptr;
		if (rejected > 0) {
			return err_set(err, 0,
				       "Turso rejected %zu buffered messages; they were not stored",
				       rejected);
		}
		return true;
	}
	f->failures++;
	f->retry_at = now_ms() + (1000U << (f->failures < 5 ? f->failures : 5));
	err_set(err, 0, "%s", err_msg(f->err));
	return false;
}

// Moves up to the limit of current lines to a new in-flight batch
static bool dispatch(struct batch_store *b, struct err **err) {
	if (b->flight != nullptr || b->current.len == 0) {
		return true;
	}
	struct flight *f = xcalloc(1, sizeof(*f));
	f->writer = b->writer;
	size_t n = b->current.len < b->limit ? b->current.len : b->limit;
	for (size_t i = 0; i < n; i++) {
		vec_push(&f->rows, b->current.data[i]);
	}
	memmove(b->current.data, b->current.data + n,
		(b->current.len - n) * sizeof(b->current.data[0]));
	b->current.len -= n;
	b->current_bytes = 0;
	for (size_t i = 0; i < b->current.len; i++) {
		b->current_bytes += db_message_insert_bytes(&b->current.data[i]);
	}
	b->flight = f;
	return start(f, err);
}

static bool has_capacity(const struct batch_store *b) {
	return b->current.len < b->limit && b->current_bytes < MAX_BUFFER_BYTES &&
	       (b->next_id <= b->end_id || b->has_next);
}

// soju-rs defers the user's events while both buffers are full; here the
// user's loop waits for the write in flight, then continues
static bool wait_capacity(struct batch_store *b, struct err **err) {
	if (has_capacity(b)) {
		return true;
	}
	if (b->next_id > b->end_id && !b->has_next) {
		if (!b->res.running && !reserve_ahead(b, err)) {
			return false;
		}
		if (b->res.running && !collect(b, err)) {
			return false;
		}
	}
	if (b->current.len >= b->limit || b->current_bytes >= MAX_BUFFER_BYTES) {
		// Preserve the two-buffer bound across transient write failures. A
		// failed flight stays in the overlay; retry it after its backoff before
		// accepting more rows into the current buffer.
		for (;;) {
			struct err *e = nullptr;
			bool ok = reap(b, true, &e);
			if (ok && b->flight == nullptr) {
				err_free(e);
				break;
			}
			if (b->flight == nullptr) {
				if (err != nullptr) {
					*err = e;
				} else {
					err_free(e);
				}
				return false;
			}
			err_free(e);
			uint64_t now = now_ms();
			if (now < b->flight->retry_at) {
				uv_sleep((unsigned)(b->flight->retry_at - now));
			}
		}
		if (!dispatch(b, err)) {
			return false;
		}
	}
	return true;
}

bool batch_store_append(struct batch_store *b, int64_t network, const char *target,
			const struct irc_msg *msg, int64_t *id, struct err **err) {
	if (!batch_store_prepare(b, err) || !wait_capacity(b, err)) {
		return false;
	}
	if (b->next_id > b->end_id) {
		if (!b->has_next) {
			return err_set(err, 0, "Turso message ID reservation pending");
		}
		b->next_id = b->next_first;
		b->end_id = b->next_last;
		b->has_next = false;
	}
	struct db_message_insert row;
	if (!db_message_insert_init(&row, b->next_id, network, target, msg, err)) {
		return false;
	}
	if (!db_message_buffer_insert(b->index, &row, err)) {
		db_message_insert_free(&row);
		return false;
	}
	*id = b->next_id++;
	if (b->current.len == 0) {
		b->since = now_ms();
	}
	b->current_bytes += db_message_insert_bytes(&row);
	vec_push(&b->current, row);
	// The line isn't held back for a write in flight
	if (b->current.len >= b->limit || b->current_bytes >= MAX_BUFFER_BYTES) {
		return dispatch(b, err);
	}
	return true;
}

bool batch_store_poll(struct batch_store *b, struct err **err) {
	if (!b->prepared) {
		return true;
	}
	if (!reserve_ahead(b, err) || !reap(b, false, err)) {
		return false;
	}
	if (b->current.len >= b->limit || now_ms() - b->since >= FLUSH_INTERVAL_MS) {
		return dispatch(b, err);
	}
	return true;
}

bool batch_store_flush(struct batch_store *b, struct err **err) {
	if (!b->prepared) {
		return true;
	}
	unsigned failures = 0;
	for (;;) {
		struct err *e = nullptr;
		if (!reap(b, true, &e)) {
			if (++failures >= 3) {
				size_t pending = b->current.len +
						 (b->flight != nullptr ? b->flight->rows.len : 0);
				err_set(err, 0,
					"failed to flush %zu buffered messages after 3 attempts: "
					"%s",
					pending, err_msg(e));
				err_free(e);
				return false;
			}
			err_free(e);
		}
		if (!dispatch(b, err)) {
			return false;
		}
		if (b->flight == nullptr && b->current.len == 0) {
			return true;
		}
	}
}
