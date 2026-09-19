//! Direct endpoint routing before outbound connectors are enabled.

use std::sync::Arc;

use bytes::Bytes;
use hyper::{Method, Request, Response, StatusCode, body::Body};

use crate::{
    config::Config,
    diagnostics::report,
    domain::target::{Target, classify},
    server::response::{ProxyBody, fixed},
};

pub(crate) struct RequestContext {
    pub(crate) config: Config,
}

impl RequestContext {
    pub(crate) fn new(config: Config) -> Self {
        Self { config }
    }
}

pub(crate) async fn handle<B>(
    request: Request<B>,
    context: Arc<RequestContext>,
) -> Response<ProxyBody>
where
    B: Body<Data = Bytes>,
{
    match classify(request.method(), request.uri()) {
        Target::Direct(path) if request.method() == Method::GET => direct(&path, &context).await,
        Target::Direct(_) => fixed(StatusCode::NOT_FOUND, None, "404 page not found\n"),
        Target::Malformed => fixed(StatusCode::BAD_REQUEST, None, "bad request\n"),
        _ => fixed(StatusCode::BAD_GATEWAY, None, "bad gateway\n"),
    }
}

async fn direct(path: &str, context: &RequestContext) -> Response<ProxyBody> {
    match path {
        "/healthz" => fixed(StatusCode::OK, None, "ok\n"),
        "/__status" => {
            let (mode, warning) = crate::domain::policy::effective_status_mode(
                context.config.allowlists.as_slice(),
                &context.config.mode_file,
            )
            .await;
            if let Some(warning) = warning {
                report(&warning);
            }
            fixed(
                StatusCode::OK,
                Some("application/json"),
                &format!("{{\"mode\":\"{mode}\"}}\n"),
            )
        }
        _ => fixed(StatusCode::NOT_FOUND, None, "404 page not found\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    fn context(allowlist: &std::path::Path, mode: &std::path::Path) -> Arc<RequestContext> {
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .unwrap();
        Arc::new(RequestContext::new(config))
    }

    #[tokio::test]
    async fn direct_endpoints_are_available_without_connectors() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        for (path, status, body) in [
            ("/healthz", StatusCode::OK, "ok\n"),
            ("/__status", StatusCode::OK, "{\"mode\":\"enforce\"}\n"),
            ("/missing", StatusCode::NOT_FOUND, "404 page not found\n"),
        ] {
            let response = handle(
                Request::builder()
                    .uri(path)
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
                context(&allowlist, &mode),
            )
            .await;
            assert_eq!(response.status(), status);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                body
            );
        }
    }
}
