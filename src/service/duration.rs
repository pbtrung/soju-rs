//! Durations formatted like Go's time.Duration, as upstream does.

/// Parses a duration like Go's time.ParseDuration. Negative durations are
/// rejected.
pub fn parse_go_duration(s: &str) -> Option<std::time::Duration> {
    let mut s = s;
    if s == "0" {
        return Some(std::time::Duration::ZERO);
    }
    if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    } else if s.starts_with('-') {
        return None;
    }
    if s.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !s.is_empty() {
        let num_len = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        if num_len == 0 {
            return None;
        }
        let v: f64 = s[..num_len].parse().ok()?;
        s = &s[num_len..];
        let unit_len = s
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(s.len());
        let unit = &s[..unit_len];
        s = &s[unit_len..];
        let mult = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += v * mult;
    }
    // Like Go's time.Duration, an int64 of nanoseconds
    if total > MAX_GO_DURATION_SECS {
        return None;
    }
    std::time::Duration::try_from_secs_f64(total).ok()
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
