//! HTTP header rules shared by both outbound paths and downstream responses.

use std::fmt;

use hyper::header::{CONNECTION, HeaderName, HeaderValue, VIA};
use hyper::{HeaderMap, Version};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderError;

impl fmt::Display for HeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid connection header")
    }
}

impl std::error::Error for HeaderError {}

/// Removes every fixed and `Connection`-nominated hop field.
pub(crate) fn sanitize_hop_by_hop(headers: &mut HeaderMap) -> Result<(), HeaderError> {
    let nominated = connection_nominations(headers)?;
    headers.remove(CONNECTION);
    for name in nominated {
        headers.remove(name);
    }
    for name in FIXED_HOP_HEADERS {
        headers.remove(*name);
    }
    Ok(())
}

/// Parses field names nominated by every `Connection` value.
pub(crate) fn connection_nominations(headers: &HeaderMap) -> Result<Vec<HeaderName>, HeaderError> {
    let mut nominated = Vec::new();
    for value in headers.get_all(CONNECTION) {
        for name in value.as_bytes().split(|byte| *byte == b',') {
            let name = trim_ows(name);
            if !valid_token(name) {
                return Err(HeaderError);
            }
            nominated.push(HeaderName::from_bytes(name).map_err(|_| HeaderError)?);
        }
    }
    Ok(nominated)
}

pub(crate) fn append_via(headers: &mut HeaderMap, received: Version) {
    let value = match received {
        Version::HTTP_10 => HeaderValue::from_static("1.0 vhrn"),
        _ => HeaderValue::from_static("1.1 vhrn"),
    };
    headers.append(VIA, value);
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn valid_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use hyper::header::CONTENT_LENGTH;
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
        sanitize_hop_by_hop(&mut headers).unwrap();
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("x-remove"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("upgrade"));
        assert!(headers.contains_key(CONTENT_LENGTH));
        assert!(headers.contains_key("x-keep"));
    }

    #[test]
    fn rejects_empty_or_malformed_connection_nominations() {
        for value in ["", "x-good,", "bad name"] {
            let mut headers = HeaderMap::new();
            headers.insert(CONNECTION, HeaderValue::from_str(value).unwrap());
            assert_eq!(sanitize_hop_by_hop(&mut headers), Err(HeaderError));
        }
    }

    #[test]
    fn appends_received_protocol_to_existing_via_chain() {
        let mut headers = HeaderMap::new();
        headers.append(VIA, HeaderValue::from_static("1.0 prior"));
        append_via(&mut headers, Version::HTTP_11);
        let values = headers
            .get_all(VIA)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, ["1.0 prior", "1.1 vhrn"]);
    }
}
