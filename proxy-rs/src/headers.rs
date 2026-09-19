//! HTTP header rules shared by both outbound paths and downstream responses.

use hyper::HeaderMap;
use hyper::header::{CONNECTION, CONTENT_LENGTH};

const FIXED_HOP_HEADERS: &[&str] = &[
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Removes hop-by-hop fields. Content length remains only because callers preserve bodies.
pub(crate) fn sanitize_hop_by_hop(headers: &mut HeaderMap) {
    let nominated = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| name.parse::<hyper::header::HeaderName>().ok())
        .collect::<Vec<_>>();
    headers.remove(CONNECTION);
    for name in nominated {
        headers.remove(name);
    }
    for name in FIXED_HOP_HEADERS {
        headers.remove(*name);
    }
    // Hyper determines transfer framing. A retained length is valid only for an unchanged body.
    let _ = headers.get(CONTENT_LENGTH);
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;

    use super::*;

    #[test]
    fn removes_connection_nominated_and_fixed_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Remove, keep-alive"));
        headers.insert("x-remove", HeaderValue::from_static("yes"));
        headers.insert("proxy-authorization", HeaderValue::from_static("secret"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        headers.insert("x-keep", HeaderValue::from_static("yes"));
        sanitize_hop_by_hop(&mut headers);
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("x-remove"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("upgrade"));
        assert!(headers.contains_key(CONTENT_LENGTH));
        assert!(headers.contains_key("x-keep"));
    }
}
