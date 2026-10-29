#include "base/cpuwork.h"
#include <stdlib.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "base/vec.h"

struct cpu_job {
	uv_work_t req;
	struct cpu_work_pool *pool;
	cpu_work_fn work;
	cpu_done_fn done;
	void *data;
};

struct cpu_work_pool {
	struct ref_node ref;
	uv_loop_t *loop;
	unsigned max_running;
	size_t running;
	VEC(struct cpu_job *) waiting; // FIFO
};

struct cpu_work_pool *cpu_work_pool_new(uv_loop_t *loop, unsigned max_running) {
	struct cpu_work_pool *p = xcalloc(1, sizeof(*p));
	p->loop = loop;
	p->max_running = max_running != 0 ? max_running : uv_available_parallelism();
	if (p->max_running == 0) {
		p->max_running = 1;
	}
	REF_TRACK(&p->ref, "cpu_work_pool");
	return p;
}

void cpu_work_pool_free(struct cpu_work_pool *p) {
	if (p == nullptr) {
		return;
	}
	vec_free(&p->waiting);
	ref_untrack(&p->ref);
	xfree(p);
}

static void run_work(uv_work_t *req) {
	struct cpu_job *j = req->data;
	j->work(j->data);
}

static void start(struct cpu_job *j);

static void after_work(uv_work_t *req, int status) {
	(void)status;
	struct cpu_job *j = req->data;
	struct cpu_work_pool *p = j->pool;
	p->running--;
	if (p->waiting.len > 0) {
		struct cpu_job *next = p->waiting.data[0];
		vec_remove(&p->waiting, 0);
		start(next);
	}
	cpu_done_fn done = j->done;
	void *data = j->data;
	xfree(j);
	if (done != nullptr) {
		done(data);
	}
}

static void start(struct cpu_job *j) {
	j->pool->running++;
	j->req.data = j;
	int rc = uv_queue_work(j->pool->loop, &j->req, run_work, after_work);
	if (rc != 0) {
		// Only fails for invalid arguments
		abort();
	}
}

bool cpu_work_submit(struct cpu_work_pool *p, cpu_work_fn work, cpu_done_fn done, void *data,
		     struct err **err) {
	if (p->running >= p->max_running && p->waiting.len >= CPU_WORK_MAX_WAITING) {
		return err_set(err, 0, "server busy, try again later");
	}
	struct cpu_job *j = xcalloc(1, sizeof(*j));
	*j = (struct cpu_job){.pool = p, .work = work, .done = done, .data = data};
	if (p->running < p->max_running) {
		start(j);
	} else {
		vec_push(&p->waiting, j);
	}
	return true;
}

size_t cpu_work_running(const struct cpu_work_pool *p) {
	return p->running;
}

size_t cpu_work_waiting(const struct cpu_work_pool *p) {
	return p->waiting.len;
}
