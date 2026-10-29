// The event loops (PLAN.md 2.1 rule 1): one libuv loop per CPU, like
// soju-rs's choice between Tokio's current_thread runtime (one CPU) and
// multi_thread one. Every object belongs to one loop; other loops reach it
// by posting a task to the owner's inbox.
#pragma once

#include <pthread.h>
#include <stdint.h>
#include <uv.h>

#include "base/ref.h"

typedef void (*loop_task_fn)(void *data);

struct loop_task;

struct sloop {
	uv_loop_t uv;
	unsigned index;
	struct loop_pool *pool;
	uv_async_t inbox_async;
	pthread_mutex_t mu;
	struct loop_task *head, *tail;
	bool inbox_closed;
	uv_thread_t thread; // loops 1..n-1
};

struct loop_pool {
	struct ref_node ref;
	unsigned n;
	struct sloop *loops;
	unsigned next_accept; // round-robin for accepted connections
};

// The number of loops for this machine: the number of CPUs, or SOJU_LOOPS
[[nodiscard]] unsigned loop_count_default(void);
void loop_pool_init(struct loop_pool *p, unsigned n);
// Runs the loops: loop 0 on the calling thread, the others on new threads.
// Returns once every loop has stopped (no active handles left) and all
// loops are closed.
void loop_pool_run(struct loop_pool *p);
// Closes the loops (each must have no handle left) and frees the pool
void loop_pool_free(struct loop_pool *p);

// The loop of the calling thread (nullptr outside the loops)
[[nodiscard]] struct sloop *loop_current(void);
// Sets the current loop of the calling thread (tests running one loop by
// hand)
void loop_set_current(struct sloop *l);
// Runs fn(data) on loop l's thread, after the tasks posted before. From l's
// own thread too, fn never runs inside the caller. If l's inbox is closed
// (shutdown), drop(data) is called instead (drop may be nullptr).
void loop_post(struct sloop *l, loop_task_fn fn, loop_task_fn drop, void *data);
// Closes l's inbox, from l's thread: tasks already posted still run, later
// ones are dropped. Once every other handle of l is closed, l stops.
void loop_close_inbox(struct sloop *l);
// The loop for a new accepted connection (round-robin)
[[nodiscard]] struct sloop *loop_pool_next(struct loop_pool *p);
// The loop owning a user
[[nodiscard]] static inline struct sloop *loop_for_user(struct loop_pool *p, int64_t user_id) {
	uint64_t u = (uint64_t)user_id;
	return &p->loops[u % p->n];
}
