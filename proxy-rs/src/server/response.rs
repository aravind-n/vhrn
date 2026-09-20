//! Infallible, bounded proxy response construction.

use crate::{connect::origin_body::SharedOriginResponse, headers::sanitize_hop_by_hop};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Method, Response, StatusCode, Version, header};
use tokio::io::{AsyncRead, AsyncWrite};

use super::http1::Http1Connection;

pub(crate) type ProxyBody = UnsyncBoxBody<Bytes, anyhow::Error>;

const TEXT: &str = "text/plain; charset=utf-8";
const MAX_GENERIC_BODY_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxyFailure {
    BadRequest,
    HeadersTooLarge,
    UnsupportedVersion,
    NotImplemented,
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
            Self::HeadersTooLarge => (
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request header fields too large\n".to_owned(),
                true,
            ),
            Self::UnsupportedVersion => (
                StatusCode::HTTP_VERSION_NOT_SUPPORTED,
                "HTTP version not supported\n".to_owned(),
                true,
            ),
            Self::NotImplemented => (
                StatusCode::NOT_IMPLEMENTED,
                "not implemented\n".to_owned(),
                true,
            ),
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

pub(crate) async fn write_response<S>(
    connection: &mut Http1Connection<S>,
    mut response: Response<ProxyBody>,
    version: Version,
    method: &Method,
    close: bool,
) -> std::io::Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (body_allowed, chunked, close, head) =
        prepare_response_head(&mut response, version, method, close);
    connection.write_all(&head).await?;
    if body_allowed {
        write_response_body(connection, response.body_mut(), chunked).await?;
    }
    connection.flush().await?;
    Ok(close)
}

fn prepare_response_head(
    response: &mut Response<ProxyBody>,
    version: Version,
    method: &Method,
    mut close: bool,
) -> (bool, bool, bool, Vec<u8>) {
    if response
        .headers()
        .get(header::CONNECTION)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"close"))
    {
        close = true;
    }
    let body_allowed = method != Method::HEAD
        && !response.status().is_informational()
        && response.status() != StatusCode::NO_CONTENT
        && response.status() != StatusCode::NOT_MODIFIED;
    let chunked = body_allowed
        && !response.headers().contains_key(header::CONTENT_LENGTH)
        && version == Version::HTTP_11;
    if chunked {
        response.headers_mut().insert(
            header::TRANSFER_ENCODING,
            hyper::http::HeaderValue::from_static("chunked"),
        );
    } else if body_allowed && !response.headers().contains_key(header::CONTENT_LENGTH) {
        close = true;
    }
    if close {
        response.headers_mut().insert(
            header::CONNECTION,
            hyper::http::HeaderValue::from_static("close"),
        );
    } else if version == Version::HTTP_10 {
        response.headers_mut().insert(
            header::CONNECTION,
            hyper::http::HeaderValue::from_static("keep-alive"),
        );
    }

    let wire_version = if version == Version::HTTP_10 {
        "HTTP/1.0"
    } else {
        "HTTP/1.1"
    };
    let reason = response.status().canonical_reason().unwrap_or("");
    let mut head =
        format!("{wire_version} {} {reason}\r\n", response.status().as_u16()).into_bytes();
    for (name, value) in response.headers() {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    (body_allowed, chunked, close, head)
}

async fn write_response_body<S>(
    connection: &mut Http1Connection<S>,
    body: &mut ProxyBody,
    chunked: bool,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = loop {
            tokio::select! {
                biased;
                frame = body.frame() => break frame,
                buffered = connection.buffer_during_response() => {
                    if !buffered? {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "downstream disconnected",
                        ));
                    }
                }
            }
        };
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(std::io::Error::other)?;
        match frame.into_data() {
            Ok(data) if !data.is_empty() && chunked => {
                connection
                    .write_all(format!("{:x}\r\n", data.len()).as_bytes())
                    .await?;
                connection.write_all(&data).await?;
                connection.write_all(b"\r\n").await?;
            }
            Ok(data) if !data.is_empty() => connection.write_all(&data).await?,
            Err(frame) if chunked => {
                if let Ok(trailers) = frame.into_trailers() {
                    connection.write_all(b"0\r\n").await?;
                    for (name, value) in trailers {
                        if let Some(name) = name {
                            connection.write_all(name.as_str().as_bytes()).await?;
                            connection.write_all(b": ").await?;
                            connection.write_all(value.as_bytes()).await?;
                            connection.write_all(b"\r\n").await?;
                        }
                    }
                    connection.write_all(b"\r\n").await?;
                    return Ok(());
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    if chunked {
        connection.write_all(b"0\r\n\r\n").await?;
    }
    Ok(())
}

pub(crate) async fn write_connect_established<S>(
    connection: &mut Http1Connection<S>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    connection
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    connection.flush().await
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
                ProxyFailure::HeadersTooLarge,
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request header fields too large\n",
                true,
            ),
            (
                ProxyFailure::UnsupportedVersion,
                StatusCode::HTTP_VERSION_NOT_SUPPORTED,
                "HTTP version not supported\n",
                true,
            ),
            (
                ProxyFailure::NotImplemented,
                StatusCode::NOT_IMPLEMENTED,
                "not implemented\n",
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
