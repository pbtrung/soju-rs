#include "net/loops.h"

#include <stdio.h>
#include <stdlib.h>

#include "base/alloc.h"
#include "base/str.h"

struct loop_task {
	struct loop_task *next;
	loop_task_fn fn, drop;
	void *data;
};

static thread_local struct sloop *current;

unsigned loop_count_default(void) {
	const char *env = getenv("SOJU_LOOPS");
	unsigned long long n;
	if (env != nullptr && str_parse_u64(str_from(env), &n) && n >= 1 && n <= 1024) {
		return (unsigned)n;
	}
	unsigned cpus = uv_available_parallelism();
	return cpus >= 1 ? cpus : 1;
}

static void drain(uv_async_t *a) {
	struct sloop *l = a->data;
	pthread_mutex_lock(&l->mu);
	struct loop_task *list = l->head;
	l->head = l->tail = nullptr;
	pthread_mutex_unlock(&l->mu);
	while (list != nullptr) {
		struct loop_task *t = list;
		list = t->next;
		t->fn(t->data);
		xfree(t);
	}
}

void loop_pool_init(struct loop_pool *p, unsigned n) {
	*p = (struct loop_pool){.n = n >= 1 ? n : 1};
	p->loops = xcalloc(p->n, sizeof(*p->loops));
	for (unsigned i = 0; i < p->n; i++) {
		struct sloop *l = &p->loops[i];
		if (uv_loop_init(&l->uv) != 0) {
			abort();
		}
		l->index = i;
		l->pool = p;
		l->uv.data = l;
		pthread_mutex_init(&l->mu, nullptr);
		if (uv_async_init(&l->uv, &l->inbox_async, drain) != 0) {
			abort();
		}
		l->inbox_async.data = l;
	}
	REF_TRACK(&p->ref, "loop_pool");
}

static void thread_main(void *arg) {
	struct sloop *l = arg;
	current = l;
	uv_run(&l->uv, UV_RUN_DEFAULT);
	current = nullptr;
}

void loop_pool_run(struct loop_pool *p) {
	for (unsigned i = 1; i < p->n; i++) {
		uv_thread_options_t opts = {.flags = UV_THREAD_HAS_STACK_SIZE,
					    .stack_size = (size_t)8 * 1024 * 1024};
		if (uv_thread_create_ex(&p->loops[i].thread, &opts, thread_main, &p->loops[i]) !=
		    0) {
			abort();
		}
	}
	thread_main(&p->loops[0]);
	for (unsigned i = 1; i < p->n; i++) {
		uv_thread_join(&p->loops[i].thread);
	}
}

void loop_pool_free(struct loop_pool *p) {
	for (unsigned i = 0; i < p->n; i++) {
		struct sloop *l = &p->loops[i];
		if (!l->inbox_closed) {
			// Never run: close the inbox here
			l->inbox_closed = true;
			uv_close((uv_handle_t *)&l->inbox_async, nullptr);
			uv_run(&l->uv, UV_RUN_DEFAULT);
		}
		int rc = uv_loop_close(&l->uv);
		if (rc != 0) {
			// A handle left open: a bug (PLAN.md 2.4)
			fprintf(stderr, "soju: loop %u: uv_loop_close: %s\n", i, uv_strerror(rc));
			uv_print_all_handles(&l->uv, stderr);
			abort();
		}
		pthread_mutex_destroy(&l->mu);
	}
	xfree(p->loops);
	ref_untrack(&p->ref);
	*p = (struct loop_pool){0};
}

struct sloop *loop_current(void) {
	return current;
}

void loop_set_current(struct sloop *l) {
	current = l;
}

void loop_post(struct sloop *l, loop_task_fn fn, loop_task_fn drop, void *data) {
	struct loop_task *t = xmalloc(sizeof(*t));
	*t = (struct loop_task){.fn = fn, .drop = drop, .data = data};
	pthread_mutex_lock(&l->mu);
	if (l->inbox_closed) {
		pthread_mutex_unlock(&l->mu);
		xfree(t);
		if (drop != nullptr) {
			drop(data);
		}
		return;
	}
	if (l->tail != nullptr) {
		l->tail->next = t;
	} else {
		l->head = t;
	}
	l->tail = t;
	pthread_mutex_unlock(&l->mu);
	uv_async_send(&l->inbox_async);
}

static void on_inbox_closed(uv_handle_t *h) {
	struct sloop *l = h->data;
	// Tasks posted between the drain and the close
	pthread_mutex_lock(&l->mu);
	struct loop_task *list = l->head;
	l->head = l->tail = nullptr;
	pthread_mutex_unlock(&l->mu);
	while (list != nullptr) {
		struct loop_task *t = list;
		list = t->next;
		if (t->drop != nullptr) {
			t->drop(t->data);
		}
		xfree(t);
	}
}

void loop_close_inbox(struct sloop *l) {
	drain(&l->inbox_async);
	pthread_mutex_lock(&l->mu);
	l->inbox_closed = true;
	pthread_mutex_unlock(&l->mu);
	uv_close((uv_handle_t *)&l->inbox_async, on_inbox_closed);
}

struct sloop *loop_pool_next(struct loop_pool *p) {
	unsigned i = __atomic_fetch_add(&p->next_accept, 1, __ATOMIC_RELAXED);
	return &p->loops[i % p->n];
}
