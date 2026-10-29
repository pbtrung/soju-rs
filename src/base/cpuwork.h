// CPU-heavy work (password hashing, key generation) on the libuv thread
// pool, like soju-rs's util::cpu_work: at most one job per CPU runs at
// once, so such work can't starve the rest of the server, and at most
// CPU_WORK_MAX_WAITING jobs wait for their turn; further ones fail with
// "server busy, try again later". A started job runs to completion.
#pragma once

#include <uv.h>

#include "base/err.h"

static constexpr size_t CPU_WORK_MAX_WAITING = 64;

struct cpu_work_pool;

// work runs on a thread pool thread, done on the loop thread
typedef void (*cpu_work_fn)(void *data);
typedef void (*cpu_done_fn)(void *data);

// max_running 0: the number of CPUs
[[nodiscard]] struct cpu_work_pool *cpu_work_pool_new(uv_loop_t *loop, unsigned max_running);
// Waiting jobs must have completed (the pool is empty)
void cpu_work_pool_free(struct cpu_work_pool *p);
[[nodiscard]] bool cpu_work_submit(struct cpu_work_pool *p, cpu_work_fn work, cpu_done_fn done,
				   void *data, struct err **err);
[[nodiscard]] size_t cpu_work_running(const struct cpu_work_pool *p);
[[nodiscard]] size_t cpu_work_waiting(const struct cpu_work_pool *p);
