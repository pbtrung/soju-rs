#include "db/dbthread.h"

#include <openssl/rand.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <time.h>

#include "base/alloc.h"
#include "base/ref.h"

struct job {
	struct job *next;
	db_job_fn run;
	db_done_fn done;
	void *data;
	// db_thread_call: signaled when run is done
	bool sync;
	bool finished;
};

struct db_thread {
	struct ref_node ref;
	struct db *db;
	uv_loop_t *loop;
	uv_thread_t thread;
	uv_async_t async;
	unsigned delay_max_ms;
	pthread_mutex_t mu;
	pthread_cond_t cond; // queue not empty, or stopping
	pthread_cond_t sync_cond; // a synchronous job finished
	struct job *queue_head, *queue_tail; // to run
	struct job *done_head, *done_tail; // to complete on the loop thread
	bool stopping;
	size_t pending; // loop thread only
};

static void push(struct job **head, struct job **tail, struct job *j) {
	j->next = nullptr;
	if (*tail != nullptr) {
		(*tail)->next = j;
	} else {
		*head = j;
	}
	*tail = j;
}

static struct job *pop(struct job **head, struct job **tail) {
	struct job *j = *head;
	if (j != nullptr) {
		*head = j->next;
		if (*head == nullptr) {
			*tail = nullptr;
		}
	}
	return j;
}

static void random_delay(unsigned max_ms) {
	uint32_t r;
	if (RAND_bytes((unsigned char *)&r, sizeof(r)) != 1) {
		abort();
	}
	unsigned ms = r % (max_ms + 1);
	struct timespec ts = {.tv_sec = ms / 1000, .tv_nsec = (long)(ms % 1000) * 1000000L};
	nanosleep(&ts, nullptr);
}

static void thread_main(void *arg) {
	struct db_thread *t = arg;
	pthread_mutex_lock(&t->mu);
	while (true) {
		struct job *j = pop(&t->queue_head, &t->queue_tail);
		if (j == nullptr) {
			if (t->stopping) {
				break;
			}
			pthread_cond_wait(&t->cond, &t->mu);
			continue;
		}
		pthread_mutex_unlock(&t->mu);
		if (t->delay_max_ms > 0) {
			random_delay(t->delay_max_ms);
		}
		j->run(t->db, j->data);
		pthread_mutex_lock(&t->mu);
		if (j->sync) {
			j->finished = true;
			pthread_cond_broadcast(&t->sync_cond);
		} else {
			push(&t->done_head, &t->done_tail, j);
			uv_async_send(&t->async);
		}
	}
	pthread_mutex_unlock(&t->mu);
}

// Loop thread: runs the completions of finished jobs
static void on_async(uv_async_t *a) {
	struct db_thread *t = a->data;
	pthread_mutex_lock(&t->mu);
	struct job *list = t->done_head;
	t->done_head = t->done_tail = nullptr;
	pthread_mutex_unlock(&t->mu);
	while (list != nullptr) {
		struct job *j = list;
		list = j->next;
		t->pending--;
		if (j->done != nullptr) {
			j->done(j->data);
		}
		xfree(j);
	}
}

static void on_close(uv_handle_t *h) {
	struct db_thread *t = h->data;
	pthread_mutex_destroy(&t->mu);
	pthread_cond_destroy(&t->cond);
	pthread_cond_destroy(&t->sync_cond);
	ref_untrack(&t->ref);
	xfree(t);
}

bool db_thread_start(uv_loop_t *loop, struct db *db, unsigned delay_max_ms, struct db_thread **out,
		     struct err **err) {
	struct db_thread *t = xcalloc(1, sizeof(*t));
	t->db = db;
	t->loop = loop;
	t->delay_max_ms = delay_max_ms;
	pthread_mutex_init(&t->mu, nullptr);
	pthread_cond_init(&t->cond, nullptr);
	pthread_cond_init(&t->sync_cond, nullptr);
	int rc = uv_async_init(loop, &t->async, on_async);
	if (rc != 0) {
		pthread_mutex_destroy(&t->mu);
		pthread_cond_destroy(&t->cond);
		pthread_cond_destroy(&t->sync_cond);
		xfree(t);
		db_close(db);
		return err_set(err, rc, "failed to start the DB thread: %s", uv_strerror(rc));
	}
	t->async.data = t;
	REF_TRACK(&t->ref, "db_thread");
	// musl's default thread stack is 128 KiB: SQLite wants more
	uv_thread_options_t opts = {.flags = UV_THREAD_HAS_STACK_SIZE,
				    .stack_size = (size_t)2 * 1024 * 1024};
	rc = uv_thread_create_ex(&t->thread, &opts, thread_main, t);
	if (rc != 0) {
		// t is freed by the close callback, on the next loop iteration
		uv_close((uv_handle_t *)&t->async, on_close);
		db_close(db);
		return err_set(err, rc, "failed to start the DB thread: %s", uv_strerror(rc));
	}
	*out = t;
	return true;
}

void db_thread_submit(struct db_thread *t, db_job_fn run, db_done_fn done, void *data) {
	struct job *j = xcalloc(1, sizeof(*j));
	*j = (struct job){.run = run, .done = done, .data = data};
	t->pending++;
	pthread_mutex_lock(&t->mu);
	push(&t->queue_head, &t->queue_tail, j);
	pthread_cond_signal(&t->cond);
	pthread_mutex_unlock(&t->mu);
}

void db_thread_call(struct db_thread *t, db_job_fn run, void *data) {
	struct job j = {.run = run, .data = data, .sync = true};
	pthread_mutex_lock(&t->mu);
	push(&t->queue_head, &t->queue_tail, &j);
	pthread_cond_signal(&t->cond);
	while (!j.finished) {
		pthread_cond_wait(&t->sync_cond, &t->mu);
	}
	pthread_mutex_unlock(&t->mu);
}

size_t db_thread_pending(const struct db_thread *t) {
	return t->pending;
}

void db_thread_stop(struct db_thread *t) {
	pthread_mutex_lock(&t->mu);
	t->stopping = true;
	pthread_cond_signal(&t->cond);
	pthread_mutex_unlock(&t->mu);
	uv_thread_join(&t->thread);
	// The thread is gone: run the remaining completions here
	on_async(&t->async);
	db_close(t->db);
	t->db = nullptr;
	uv_close((uv_handle_t *)&t->async, on_close);
}
