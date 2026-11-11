//! Durations formatted like Go's time.Duration, as upstream does.

/// Parses a duration like Go's time.ParseDuration. Negative durations are
/// rejected.
pub fn parse_go_duration(s: &str) -> Option<std::time::Duration> {
    let mut s = s.strip_prefix('+').unwrap_or(s);
    if s == "0" {
        return Some(std::time::Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut total = 0u64;
    while !s.is_empty() {
        let num_len = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        let number = &s[..num_len];
        let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
        if integer.is_empty() && fraction.is_empty()
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        s = &s[num_len..];
        let unit_len = s
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(s.len());
        let unit = &s[..unit_len];
        s = &s[unit_len..];
        let multiplier = match unit {
            "ns" => 1u64,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        let integer: u64 = if integer.is_empty() {
            0
        } else {
            integer.parse().ok()?
        };
        // Evaluate from right to left to keep arbitrarily long fractions exact
        // without overflowing. Each component truncates sub-nanoseconds.
        let fractional = fraction.bytes().rev().fold(0u64, |n, digit| {
            (n + u64::from(digit - b'0') * multiplier) / 10
        });
        total = total
            .checked_add(integer.checked_mul(multiplier)?)?
            .checked_add(fractional)?;
        if total > i64::MAX as u64 {
            return None;
        }
    }
    Some(std::time::Duration::from_nanos(total))
}

/// The largest Go time.Duration, in seconds (about 292 years).
pub const MAX_GO_DURATION_SECS: f64 = i64::MAX as f64 / 1e9;

/// Formats a duration like Go's time.Duration.String, e.g. "1h30m0s", so that
/// it can be passed back to parse_go_duration.
pub fn format_go_duration(d: std::time::Duration) -> String {
    if d.is_zero() {
        return "0s".to_owned();
    }
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&s.to_string());
    let nanos = d.subsec_nanos();
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push('s');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formatted_durations_roundtrip_across_the_integer_range() {
        for nanos in (0..=1_000).map(|i| i64::MAX as u64 / 1_000 * i) {
            let duration = Duration::from_nanos(nanos);
            assert_eq!(
                parse_go_duration(&format_go_duration(duration)),
                Some(duration)
            );
        }
    }

    #[test]
    fn exact_nanoseconds_and_go_range() {
        for (input, nanos) in [
            ("+0", 0),
            (".5s", 500_000_000),
            ("1.s", 1_000_000_000),
            ("9007199254740993ns", 9_007_199_254_740_993),
            ("2562047h47m16.854775807s", i64::MAX as u64),
            ("0.0000000000002777777777777777777777777778h", 1),
            ("0.6ns0.6ns", 0),
        ] {
            assert_eq!(
                parse_go_duration(input),
                Some(Duration::from_nanos(nanos)),
                "{input}"
            );
        }
        for input in [
            "9223372036854775808ns",
            "2562047h47m16.854775808s",
            "1..2s",
            ".s",
            "1",
            "-1s",
            "999999999999999999999999h",
        ] {
            assert_eq!(parse_go_duration(input), None, "{input}");
        }
        let d = Duration::from_nanos(i64::MAX as u64);
        assert_eq!(parse_go_duration(&format_go_duration(d)), Some(d));
    }
}
