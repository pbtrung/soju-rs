// Points in time like chrono's DateTime<Utc>, which soju-rs uses: seconds
// since the Unix epoch plus nanoseconds, over chrono's whole range of years
// (-262144 to 262143). As in chrono, a leap second ("23:59:60") is the
// second :59 with nsec >= 1e9 (up to 1,999,999,999).
#pragma once

#include <stdint.h>

#include "base/str.h"

struct timestamp {
	int64_t sec;
	uint32_t nsec;
};

// The current time (CLOCK_REALTIME)
[[nodiscard]] struct timestamp timestamp_now(void);
[[nodiscard]] int timestamp_cmp(struct timestamp a, struct timestamp b);
[[nodiscard]] struct timestamp timestamp_from_unix(int64_t sec, uint32_t nsec);
// Milliseconds since the epoch (DateTime::timestamp_millis)
[[nodiscard]] int64_t timestamp_millis(struct timestamp t);

struct civil {
	int64_t year;
	unsigned month, day, hour, minute, second; // second: 60 in a leap second
	uint32_t nsec; // < 1e9
};

[[nodiscard]] struct civil timestamp_to_civil(struct timestamp t);
// Days since 1970-01-01 of a proleptic Gregorian date
[[nodiscard]] int64_t days_from_civil(int64_t y, unsigned m, unsigned d);
[[nodiscard]] bool civil_date_valid(int64_t y, unsigned m, unsigned d);

// The IRCv3 server-time layout, like xirc::format_server_time:
// "2024-01-02T03:04:05.678Z" (chrono's %Y-%m-%dT%H:%M:%S%.3fZ)
void format_server_time(struct strbuf *out, struct timestamp t);
[[nodiscard]] char *format_server_time_cstr(struct timestamp t);
// xirc::parse_server_time: the fractional seconds may have 1 to 9 digits
// or be omitted
[[nodiscard]] bool parse_server_time(struct str s, struct timestamp *out);
