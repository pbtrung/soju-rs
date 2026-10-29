#include "base/timeutil.h"

#include <stdckdint.h>
#include <time.h>

#include "base/utf8.h"

// chrono's NaiveDate range
static constexpr int64_t MIN_YEAR = -262143;
static constexpr int64_t MAX_YEAR = 262142;
static constexpr uint32_t NSEC_PER_SEC = 1000000000;

struct timestamp timestamp_now(void) {
	struct timespec ts;
	clock_gettime(CLOCK_REALTIME, &ts);
	return (struct timestamp){.sec = ts.tv_sec, .nsec = (uint32_t)ts.tv_nsec};
}

int timestamp_cmp(struct timestamp a, struct timestamp b) {
	if (a.sec != b.sec) {
		return a.sec < b.sec ? -1 : 1;
	}
	if (a.nsec != b.nsec) {
		return a.nsec < b.nsec ? -1 : 1;
	}
	return 0;
}

struct timestamp timestamp_from_unix(int64_t sec, uint32_t nsec) {
	return (struct timestamp){.sec = sec, .nsec = nsec};
}

int64_t timestamp_millis(struct timestamp t) {
	return t.sec * 1000 + (int64_t)(t.nsec / 1000000);
}

static int64_t floor_div(int64_t a, int64_t b) {
	int64_t q = a / b;
	if ((a % b != 0) && ((a < 0) != (b < 0))) {
		q--;
	}
	return q;
}

// Howard Hinnant's days_from_civil / civil_from_days
int64_t days_from_civil(int64_t y, unsigned m, unsigned d) {
	y -= m <= 2;
	int64_t era = floor_div(y, 400);
	int64_t yoe = y - era * 400;
	int64_t mp = (m + 9) % 12;
	int64_t doy = (153 * mp + 2) / 5 + d - 1;
	int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	return era * 146097 + doe - 719468;
}

static void civil_from_days(int64_t z, int64_t *y, unsigned *m, unsigned *d) {
	z += 719468;
	int64_t era = floor_div(z, 146097);
	int64_t doe = z - era * 146097;
	int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
	int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	int64_t mp = (5 * doy + 2) / 153;
	*d = (unsigned)(doy - (153 * mp + 2) / 5 + 1);
	*m = (unsigned)(mp < 10 ? mp + 3 : mp - 9);
	*y = yoe + era * 400 + (*m <= 2);
}

static bool is_leap(int64_t y) {
	return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
}

bool civil_date_valid(int64_t y, unsigned m, unsigned d) {
	static const unsigned days[] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
	if (y < MIN_YEAR || y > MAX_YEAR || m < 1 || m > 12 || d < 1) {
		return false;
	}
	unsigned max = days[m - 1] + (m == 2 && is_leap(y));
	return d <= max;
}

struct civil timestamp_to_civil(struct timestamp t) {
	struct civil c = {0};
	int64_t days = floor_div(t.sec, 86400);
	int64_t secs = t.sec - days * 86400;
	civil_from_days(days, &c.year, &c.month, &c.day);
	c.hour = (unsigned)(secs / 3600);
	c.minute = (unsigned)(secs / 60 % 60);
	c.second = (unsigned)(secs % 60) + t.nsec / NSEC_PER_SEC;
	c.nsec = t.nsec % NSEC_PER_SEC;
	return c;
}

// chrono's %Y: four digits for years 0 to 9999, else a sign and at least
// four digits
static void format_year(struct strbuf *out, int64_t y) {
	if (y >= 0 && y <= 9999) {
		strbuf_printf(out, "%04lld", (long long)y);
	} else {
		strbuf_printf(out, "%+05lld", (long long)y);
	}
}

void format_server_time(struct strbuf *out, struct timestamp t) {
	struct civil c = timestamp_to_civil(t);
	format_year(out, c.year);
	strbuf_printf(out, "-%02u-%02uT%02u:%02u:%02u.%03uZ", c.month, c.day, c.hour, c.minute,
		      c.second, (unsigned)(c.nsec / 1000000));
}

char *format_server_time_cstr(struct timestamp t) {
	struct strbuf sb = STRBUF_INIT;
	format_server_time(&sb, t);
	return strbuf_take(&sb);
}

// chrono's numeric item: leading whitespace, then 1 to max digits (a sign
// and any number of digits for a signed item)
static bool scan_number(struct str *s, size_t max, bool is_signed, int64_t *out) {
	*s = utf8_trim_start(*s);
	bool neg = false;
	if (is_signed && s->len > 0 && (s->p[0] == '-' || s->p[0] == '+')) {
		neg = s->p[0] == '-';
		*s = str_slice(*s, 1, s->len);
		max = SIZE_MAX;
	}
	int64_t n = 0;
	size_t i = 0;
	for (; i < s->len && i < max; i++) {
		char c = s->p[i];
		if (c < '0' || c > '9') {
			break;
		}
		if (ckd_mul(&n, n, 10) || ckd_add(&n, n, c - '0')) {
			return false;
		}
	}
	if (i == 0) {
		return false;
	}
	*s = str_slice(*s, i, s->len);
	*out = neg ? -n : n;
	return true;
}

static bool scan_literal(struct str *s, char c) {
	if (s->len == 0 || s->p[0] != c) {
		return false;
	}
	*s = str_slice(*s, 1, s->len);
	return true;
}

// NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
static bool parse_naive(struct str s, struct timestamp *out) {
	int64_t y, mo, d, h, mi, sec;
	if (!scan_number(&s, 4, true, &y) || !scan_literal(&s, '-') ||
	    !scan_number(&s, 2, false, &mo) || !scan_literal(&s, '-') ||
	    !scan_number(&s, 2, false, &d) || !scan_literal(&s, 'T') ||
	    !scan_number(&s, 2, false, &h) || !scan_literal(&s, ':') ||
	    !scan_number(&s, 2, false, &mi) || !scan_literal(&s, ':') ||
	    !scan_number(&s, 2, false, &sec) || s.len != 0) {
		return false;
	}
	if (y < INT32_MIN || y > INT32_MAX || mo < 1 || mo > 12 || d < 1 || d > 31 || h > 23 ||
	    mi > 59 || sec > 60) {
		return false;
	}
	if (!civil_date_valid(y, (unsigned)mo, (unsigned)d)) {
		return false;
	}
	uint32_t nsec = 0;
	if (sec == 60) {
		sec = 59;
		nsec = NSEC_PER_SEC;
	}
	out->sec = days_from_civil(y, (unsigned)mo, (unsigned)d) * 86400 + h * 3600 + mi * 60 + sec;
	out->nsec = nsec;
	return true;
}

bool parse_server_time(struct str s, struct timestamp *out) {
	if (s.len == 0 || s.p[s.len - 1] != 'Z') {
		return false;
	}
	s.len--;
	struct str base = s, frac = STR_EMPTY;
	bool has_frac = str_split_once(s, '.', &base, &frac);
	struct timestamp t;
	if (!parse_naive(base, &t) || base.len != 19) {
		return false;
	}
	uint32_t nanos = 0;
	if (has_frac) {
		if (frac.len == 0 || frac.len > 9) {
			return false;
		}
		for (size_t i = 0; i < 9; i++) {
			char c = i < frac.len ? frac.p[i] : '0';
			if (c < '0' || c > '9') {
				return false;
			}
			nanos = nanos * 10 + (uint32_t)(c - '0');
		}
	}
	// naive + Duration::nanoseconds(nanos): within a leap second, chrono
	// keeps the sum in it (nsec stays below 2e9)
	t.nsec += nanos;
	*out = t;
	return true;
}
