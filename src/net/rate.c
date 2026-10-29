#include "net/rate.h"

#include <openssl/rand.h>
#include <stdckdint.h>
#include <stdlib.h>

void backoffer_init(struct backoffer *b, uint64_t min, uint64_t max, uint64_t jitter) {
	*b = (struct backoffer){.min = min, .max = max, .jitter = jitter};
}

void backoffer_reset(struct backoffer *b) {
	b->n = 0;
}

static uint64_t sat_mul(uint64_t a, uint64_t b) {
	uint64_t r;
	return ckd_mul(&r, a, b) ? UINT64_MAX : r;
}

// A uniform random number below n (n > 0)
static uint64_t random_below(uint64_t n) {
	uint64_t limit = UINT64_MAX - UINT64_MAX % n;
	uint64_t r;
	do {
		if (RAND_bytes((unsigned char *)&r, sizeof(r)) != 1) {
			abort();
		}
	} while (r >= limit);
	return r % n;
}

uint64_t backoffer_next_delay(struct backoffer *b) {
	if (b->n == 0) {
		b->n = 1;
		return 0;
	}
	uint64_t d = sat_mul(b->min, b->n);
	if (d > b->max) {
		d = b->max;
	} else {
		uint64_t n2 = sat_mul(b->n, 2);
		b->n = n2 > UINT32_MAX ? UINT32_MAX : (uint32_t)n2;
	}
	if (b->jitter != 0) {
		d += random_below(b->jitter);
	}
	return d;
}

void rate_limiter_init(struct rate_limiter *rl, uint64_t interval_ns, uint32_t burst,
		       uint64_t now_ns) {
	*rl = (struct rate_limiter){
		.interval = (double)interval_ns / 1e9,
		.burst = burst,
		.tokens = burst,
		.last = now_ns,
	};
}

uint64_t rate_limiter_reserve(struct rate_limiter *rl, uint64_t now_ns) {
	double elapsed = now_ns > rl->last ? (double)(now_ns - rl->last) / 1e9 : 0;
	rl->last = now_ns;
	double t = rl->tokens + elapsed / rl->interval;
	rl->tokens = t < rl->burst ? t : rl->burst;
	rl->tokens -= 1.0;
	if (rl->tokens < 0.0) {
		// Duration::from_secs_f64 rounds to the nearest nanosecond
		return (uint64_t)(-rl->tokens * rl->interval * 1e9 + 0.5);
	}
	return 0;
}
