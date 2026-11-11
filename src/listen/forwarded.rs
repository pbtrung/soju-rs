//! Forwarded addresses and certificates supplied by trusted HTTP proxies.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use std::net::SocketAddr;

/// Parses a Forwarded header (RFC 7239), or the X-Forwarded-For header.
/// Returns the client address the proxy forwarded, if any.
pub(super) fn parse_forwarded(headers: &hyper::HeaderMap) -> Option<String> {
    let Some(forwarded) = headers.get("Forwarded").and_then(|v| v.to_str().ok()) else {
        return parse_x_forwarded_for(headers);
    };
    // Like Go's mime.ParseMediaType("hack; " + forwarded): a single element
    // of semicolon-separated key=value pairs
    let mut result = None;
    for part in forwarded.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=')?;
        let k = k.trim().to_lowercase();
        let v = v.trim();
        if !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
        {
            return None;
        }
        let v = if let Some(q) = v.strip_prefix('"') {
            q.strip_suffix('"')?.replace("\\\"", "\"")
        } else {
            if v.contains([',', ' ', '"']) {
                return None;
            }
            v.to_owned()
        };
        if k == "for" {
            result = Some(v);
        }
    }
    // The address ends up in IRC prefixes, where spaces would split them
    result.filter(|v| !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || c.is_control()))
}

/// Returns the last address of X-Forwarded-For, the one added by the
/// trusted proxy: the header is a list ("client, proxy1, ..."), possibly
/// split over several header lines, whose other elements came from the
/// client or other proxies.
///
/// Upstream uses the whole header as the client address, so a list ends up
/// in IRC prefixes with spaces in it, which breaks clients.
fn parse_x_forwarded_for(headers: &hyper::HeaderMap) -> Option<String> {
    let last = headers
        .get_all("X-Forwarded-For")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|v| !v.is_empty())?;
    let valid = last.parse::<std::net::IpAddr>().is_ok() || last.parse::<SocketAddr>().is_ok();
    valid.then(|| last.to_owned())
}

pub(super) fn parse_client_cert(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix(':')?.strip_suffix(':')?;
    BASE64.decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;
    #[test]
    fn forwarded() {
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "Forwarded",
            HeaderValue::from_static("for=\"[2001:db8::1]:1234\";proto=https"),
        );
        assert_eq!(parse_forwarded(&h).as_deref(), Some("[2001:db8::1]:1234"));
        let mut h = hyper::HeaderMap::new();
        h.insert("X-Forwarded-For", HeaderValue::from_static("1.2.3.4"));
        assert_eq!(parse_forwarded(&h).as_deref(), Some("1.2.3.4"));
        // A list: the last element was added by the trusted proxy
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "X-Forwarded-For",
            HeaderValue::from_static("6.6.6.6, 1.2.3.4 , 127.0.0.1"),
        );
        assert_eq!(parse_forwarded(&h).as_deref(), Some("127.0.0.1"));
        // Several header lines form a single list
        let mut h = hyper::HeaderMap::new();
        h.append("X-Forwarded-For", HeaderValue::from_static("6.6.6.6"));
        h.append("X-Forwarded-For", HeaderValue::from_static("2001:db8::1,"));
        assert_eq!(parse_forwarded(&h).as_deref(), Some("2001:db8::1"));
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "X-Forwarded-For",
            HeaderValue::from_static("[2001:db8::1]:443"),
        );
        assert_eq!(parse_forwarded(&h).as_deref(), Some("[2001:db8::1]:443"));
        // Anything else falls back to the connection's address
        for v in ["", " , ", "not an ip", "1.2.3.4 5.6.7.8", "unknown"] {
            let mut h = hyper::HeaderMap::new();
            h.insert("X-Forwarded-For", HeaderValue::from_str(v).unwrap());
            assert_eq!(parse_forwarded(&h), None, "{v:?}");
        }
        let mut h = hyper::HeaderMap::new();
        h.insert("Forwarded", HeaderValue::from_static("for=\"a b\""));
        assert_eq!(parse_forwarded(&h), None);
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "Forwarded",
            HeaderValue::from_static("for=1.1.1.1, for=2.2.2.2"),
        );
        assert_eq!(parse_forwarded(&h), None);
    }

    #[test]
    fn client_cert_header() {
        assert_eq!(parse_client_cert(":AAEC:"), Some(vec![0, 1, 2]));
        assert_eq!(parse_client_cert("AAEC"), None);
    }
}
