// Exponential backoff and token-bucket rate limiting (soju-rs rate.rs).
// Durations are in nanoseconds; the limiter only computes waits, the caller
// arms a timer.
#pragma once

#include <stdint.h>

struct backoffer {
	uint64_t min, max, jitter;
	uint32_t n;
};

void backoffer_init(struct backoffer *b, uint64_t min, uint64_t max, uint64_t jitter);
void backoffer_reset(struct backoffer *b);
[[nodiscard]] uint64_t backoffer_next_delay(struct backoffer *b);

// Like golang.org/x/time/rate with a limit of one event every interval and
// the given burst; the bucket starts full
struct rate_limiter {
	double interval; // seconds
	double burst;
	double tokens;
	uint64_t last; // monotonic nanoseconds
};

void rate_limiter_init(struct rate_limiter *rl, uint64_t interval_ns, uint32_t burst,
		       uint64_t now_ns);
// Takes a token at time now; returns how long to wait before the event is
// allowed (0: right away)
[[nodiscard]] uint64_t rate_limiter_reserve(struct rate_limiter *rl, uint64_t now_ns);
