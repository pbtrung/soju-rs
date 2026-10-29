// Ported from soju-rs src/rate.rs tests
#include "testsupport.h"

#include "net/rate.h"

static constexpr uint64_t SEC = 1000000000;

static void test_backoff(void **state) {
	(void)state;
	struct backoffer b;
	backoffer_init(&b, 1 * SEC, 5 * SEC, 0);
	const uint64_t want[] = {0, 1, 2, 4, 5, 5};
	for (size_t i = 0; i < 6; i++) {
		assert_int_equal(backoffer_next_delay(&b) / SEC, want[i]);
	}
	backoffer_reset(&b);
	assert_int_equal(backoffer_next_delay(&b), 0);
	backoffer_init(&b, 1 * SEC, 5 * SEC, 1 * SEC);
	(void)backoffer_next_delay(&b);
	uint64_t d = backoffer_next_delay(&b);
	assert_true(d >= 1 * SEC && d < 2 * SEC);
}

// The Rust test runs on a paused Tokio clock; here the clock is passed in
static void test_limiter(void **state) {
	(void)state;
	struct rate_limiter rl;
	uint64_t now = 1000 * SEC;
	rate_limiter_init(&rl, 2 * SEC, 3, now);
	for (int i = 0; i < 3; i++) {
		assert_int_equal(rate_limiter_reserve(&rl, now), 0);
	}
	uint64_t wait = rate_limiter_reserve(&rl, now);
	assert_true(wait >= 2 * SEC - 1 && wait <= 2 * SEC);
	now += wait;
	wait = rate_limiter_reserve(&rl, now);
	assert_true(wait >= 2 * SEC - 1 && wait <= 2 * SEC);
}

T_MAIN(cmocka_unit_test(test_backoff), cmocka_unit_test(test_limiter))
