//! Infallible proxy response construction.
use crate::{connect::origin_body::SharedOriginResponse, headers::sanitize_hop_by_hop};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Response, StatusCode};
pub(crate) type ProxyBody = UnsyncBoxBody<Bytes, anyhow::Error>;
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
pub(crate) fn origin(origin: SharedOriginResponse) -> Response<ProxyBody> {
    let mut response = Response::new(origin.body.boxed_unsync());
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
    async fn origin_response_preserves_status_body_and_ordinary_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-origin", HeaderValue::from_static("present"));
        let response = origin(SharedOriginResponse {
            status: StatusCode::CREATED,
            headers,
            body: http_body_util::Full::new(Bytes::from_static(b"origin"))
                .map_err(|never| match never {})
                .boxed_unsync(),
        });
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
        let response = origin(SharedOriginResponse {
            status: StatusCode::OK,
            headers,
            body: http_body_util::Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed_unsync(),
        });
        assert!(!response.headers().contains_key(CONNECTION));
        assert!(!response.headers().contains_key("x-remove"));
        assert!(!response.headers().contains_key("transfer-encoding"));
        assert_eq!(response.headers()["keep"], "yes");
    }
}
