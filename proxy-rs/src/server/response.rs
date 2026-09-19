//! Infallible proxy response construction.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
