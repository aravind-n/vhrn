//! Infallible, bounded proxy response construction.

use crate::{connect::origin_body::SharedOriginResponse, headers::sanitize_hop_by_hop};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Response, StatusCode, header};

pub(crate) type ProxyBody = UnsyncBoxBody<Bytes, anyhow::Error>;

const TEXT: &str = "text/plain; charset=utf-8";
const MAX_GENERIC_BODY_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxyFailure {
    BadRequest,
    PublicDenied(String),
    LocalDenied(String),
    HttpsRequiresConnect,
    ReportLogUnavailable,
    BadGateway,
}

impl ProxyFailure {
    fn representation(self) -> (StatusCode, String, bool) {
        match self {
            Self::BadRequest => (StatusCode::BAD_REQUEST, "bad request\n".to_owned(), true),
            Self::PublicDenied(host) => (
                StatusCode::FORBIDDEN,
                format!("blocked by vhrn egress policy: {host}\n"),
                false,
            ),
            Self::LocalDenied(authority) => (
                StatusCode::FORBIDDEN,
                format!("blocked by vhrn local policy: {authority}\n"),
                false,
            ),
            Self::HttpsRequiresConnect => (
                StatusCode::BAD_REQUEST,
                "HTTPS requires CONNECT\n".to_owned(),
                true,
            ),
            Self::ReportLogUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy temporarily unavailable\n".to_owned(),
                true,
            ),
            Self::BadGateway => (StatusCode::BAD_GATEWAY, "bad gateway\n".to_owned(), false),
        }
    }
}

pub(crate) fn failure(failure: ProxyFailure, head: bool) -> Response<ProxyBody> {
    let (status, mut representation, close) = failure.representation();
    if representation.len() > MAX_GENERIC_BODY_BYTES
        || !representation.ends_with('\n')
        || representation[..representation.len() - 1].contains(['\r', '\n'])
    {
        representation.clear();
        representation.push_str("proxy error\n");
    }
    let length = representation.len();
    let body = if head {
        Bytes::new()
    } else {
        Bytes::from(representation)
    };
    let mut response = Response::new(
        Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        hyper::http::HeaderValue::from_static(TEXT),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        hyper::http::HeaderValue::from_str(&length.to_string())
            .expect("bounded response length is a valid header"),
    );
    if close {
        response.headers_mut().insert(
            header::CONNECTION,
            hyper::http::HeaderValue::from_static("close"),
        );
    }
    response
}

pub(crate) fn fixed(
    status: StatusCode,
    content_type: Option<&'static str>,
    body: &str,
) -> Response<ProxyBody> {
    let mut response = Response::new(
        Full::new(Bytes::copy_from_slice(body.as_bytes()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    if let Some(value) = content_type {
        response.headers_mut().insert(
            hyper::http::header::CONTENT_TYPE,
            hyper::http::HeaderValue::from_static(value),
        );
    }
    response
}

pub(crate) fn represented(
    status: StatusCode,
    content_type: Option<&'static str>,
    representation: &str,
    head: bool,
) -> Response<ProxyBody> {
    let body = if head {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(representation.as_bytes())
    };
    let mut response = Response::new(
        Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    if let Some(value) = content_type {
        response.headers_mut().insert(
            hyper::http::header::CONTENT_TYPE,
            hyper::http::HeaderValue::from_static(value),
        );
    }
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        hyper::http::HeaderValue::from_str(&representation.len().to_string())
            .expect("response length is a valid header"),
    );
    response
}
pub(crate) fn origin(origin: SharedOriginResponse, head: bool) -> Response<ProxyBody> {
    let body = if head {
        Full::new(Bytes::new())
            .map_err(|never| match never {})
            .boxed_unsync()
    } else {
        origin.body.boxed_unsync()
    };
    let mut response = Response::new(body);
    *response.status_mut() = origin.status;
    let mut headers = origin.headers;
    sanitize_hop_by_hop(&mut headers);
    response.headers_mut().extend(headers);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::header::{CONNECTION, HeaderName, HeaderValue};

    #[tokio::test]
    async fn fixed_response_has_exact_status_content_type_and_body() {
        let response = fixed(StatusCode::FORBIDDEN, Some("text/plain"), "forbidden\n");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()[hyper::header::CONTENT_TYPE],
            "text/plain"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "forbidden\n");
    }

    #[tokio::test]
    async fn failures_have_exact_status_headers_and_bytes_including_head() {
        let cases = [
            (
                ProxyFailure::BadRequest,
                StatusCode::BAD_REQUEST,
                "bad request\n",
                true,
            ),
            (
                ProxyFailure::HttpsRequiresConnect,
                StatusCode::BAD_REQUEST,
                "HTTPS requires CONNECT\n",
                true,
            ),
            (
                ProxyFailure::PublicDenied("example.com".to_owned()),
                StatusCode::FORBIDDEN,
                "blocked by vhrn egress policy: example.com\n",
                false,
            ),
            (
                ProxyFailure::LocalDenied("localhost:443".to_owned()),
                StatusCode::FORBIDDEN,
                "blocked by vhrn local policy: localhost:443\n",
                false,
            ),
            (
                ProxyFailure::ReportLogUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy temporarily unavailable\n",
                true,
            ),
            (
                ProxyFailure::BadGateway,
                StatusCode::BAD_GATEWAY,
                "bad gateway\n",
                false,
            ),
        ];
        for (failure, status, body, close) in cases {
            for head in [false, true] {
                let response = super::failure(failure.clone(), head);
                assert_eq!(response.status(), status);
                assert_eq!(response.headers()[header::CONTENT_TYPE], TEXT);
                assert_eq!(
                    response.headers()[header::CONTENT_LENGTH],
                    body.len().to_string()
                );
                assert_eq!(response.headers().contains_key(header::CONNECTION), close);
                let actual = response.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(actual, if head { "" } else { body });
            }
        }
    }

    #[tokio::test]
    async fn dynamic_failure_content_cannot_escape_the_one_line_bound() {
        let response = failure(ProxyFailure::PublicDenied("x\n".repeat(600)), false);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "12");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "proxy error\n"
        );
    }

    #[tokio::test]
    async fn origin_response_preserves_status_body_and_ordinary_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-origin", HeaderValue::from_static("present"));
        let response = origin(
            SharedOriginResponse {
                status: StatusCode::CREATED,
                headers,
                body: http_body_util::Full::new(Bytes::from_static(b"origin"))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            },
            false,
        );
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-origin"], "present");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "origin"
        );
    }

    #[test]
    fn origin_response_strips_hop_by_hop_and_connection_named_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("x-remove"));
        headers.insert("x-remove", HeaderValue::from_static("no"));
        headers.insert("keep", HeaderValue::from_static("yes"));
        headers.insert(
            HeaderName::from_static("transfer-encoding"),
            HeaderValue::from_static("chunked"),
        );
        let response = origin(
            SharedOriginResponse {
                status: StatusCode::OK,
                headers,
                body: http_body_util::Full::new(Bytes::new())
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            },
            false,
        );
        assert!(!response.headers().contains_key(CONNECTION));
        assert!(!response.headers().contains_key("x-remove"));
        assert!(!response.headers().contains_key("transfer-encoding"));
        assert_eq!(response.headers()["keep"], "yes");
    }

    #[tokio::test]
    async fn head_origin_preserves_representation_headers_without_body_bytes() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("6"));
        let response = origin(
            SharedOriginResponse {
                status: StatusCode::OK,
                headers,
                body: http_body_util::Full::new(Bytes::from_static(b"origin"))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            },
            true,
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
}
