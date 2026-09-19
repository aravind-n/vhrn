//! Request-head authorization and public origin dispatch.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Incoming},
};

use crate::{
    config::Config,
    connect::public::PublicConnector,
    diagnostics::{DenialDestination, DenialRecorder, report},
    domain::{
        policy::decide_public,
        target::{PublicTarget, Target, classify},
    },
    server::response::{ProxyBody, fixed, origin},
};

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct RequestContext {
    pub(crate) config: Config,
    pub(crate) public: PublicConnector,
}

impl RequestContext {
    pub(crate) fn new(config: Config, public: PublicConnector) -> Self {
        Self { config, public }
    }
}

enum HttpRoute {
    Direct(String),
    Public(PublicTarget),
    Local,
    Malformed,
}

enum HeadDecision {
    Forward(HttpRoute),
    Respond(Response<ProxyBody>),
}

fn classify_http(method: &Method, uri: &hyper::Uri) -> HttpRoute {
    match classify(method, uri) {
        Target::Direct(value) => HttpRoute::Direct(value),
        Target::PublicHttp(value) => HttpRoute::Public(value),
        Target::LocalHttp(_) => HttpRoute::Local,
        Target::Malformed | Target::PublicConnect(_) | Target::LocalConnect(_) => {
            HttpRoute::Malformed
        }
    }
}

pub(crate) async fn handle(
    request: Request<Incoming>,
    context: Arc<RequestContext>,
) -> Response<ProxyBody> {
    if request.method() == Method::CONNECT {
        return fixed(StatusCode::BAD_REQUEST, None, "bad request\n");
    }
    handle_http(request, context).await
}

async fn handle_http<B>(request: Request<B>, context: Arc<RequestContext>) -> Response<ProxyBody>
where
    B: Body<Data = Bytes> + Unpin,
{
    let (parts, body) = request.into_parts();
    match authorize(&parts, &context).await {
        HeadDecision::Respond(response) => response,
        HeadDecision::Forward(HttpRoute::Direct(path)) => direct(&path, &context).await,
        HeadDecision::Forward(HttpRoute::Malformed) => {
            fixed(StatusCode::BAD_REQUEST, None, "bad request\n")
        }
        HeadDecision::Forward(route) => {
            match collect_with_limits(body, BODY_TIMEOUT, MAX_BODY_BYTES).await {
                Ok(body) => {
                    dispatch(Request::from_parts(parts, Full::new(body)), route, context).await
                }
                Err(()) => fixed(StatusCode::BAD_REQUEST, None, "bad request\n"),
            }
        }
    }
}

async fn authorize(parts: &hyper::http::request::Parts, context: &RequestContext) -> HeadDecision {
    let route = classify_http(&parts.method, &parts.uri);
    match &route {
        HttpRoute::Direct(_) if parts.method == Method::GET => HeadDecision::Forward(route),
        HttpRoute::Direct(_) => {
            HeadDecision::Respond(fixed(StatusCode::NOT_FOUND, None, "404 page not found\n"))
        }
        HttpRoute::Malformed => HeadDecision::Forward(route),
        HttpRoute::Local => {
            HeadDecision::Respond(fixed(StatusCode::FORBIDDEN, None, "forbidden\n"))
        }
        HttpRoute::Public(target) => {
            let decision = match decide_public(
                context.config.allowlists.as_slice(),
                &context.config.mode_file,
                target.host(),
            )
            .await
            {
                Ok((value, warning)) => {
                    if let Some(warning) = warning {
                        report(&warning);
                    }
                    value
                }
                Err(error) => {
                    report(&error);
                    record_best_effort(context, target).await;
                    return HeadDecision::Respond(fixed(
                        StatusCode::FORBIDDEN,
                        None,
                        "forbidden\n",
                    ));
                }
            };
            if !decision.allowed {
                record_best_effort(context, target).await;
                return HeadDecision::Respond(fixed(StatusCode::FORBIDDEN, None, "forbidden\n"));
            }
            if decision.record_denial
                && let Err(error) = record(context, target).await
            {
                report(&error);
                return HeadDecision::Respond(fixed(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    "internal server error\n",
                ));
            }
            HeadDecision::Forward(route)
        }
    }
}

async fn dispatch(
    request: Request<Full<Bytes>>,
    route: HttpRoute,
    context: Arc<RequestContext>,
) -> Response<ProxyBody> {
    match route {
        HttpRoute::Public(target) => match context.public.send(target, request).await {
            Ok(value) => origin(value),
            Err(error) => {
                report(&error);
                fixed(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            }
        },
        HttpRoute::Local => fixed(StatusCode::FORBIDDEN, None, "forbidden\n"),
        HttpRoute::Direct(_) | HttpRoute::Malformed => {
            fixed(StatusCode::BAD_REQUEST, None, "bad request\n")
        }
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

async fn collect_with_limits<B: Body<Data = Bytes> + Unpin>(
    body: B,
    timeout: Duration,
    limit: usize,
) -> Result<Bytes, ()> {
    tokio::time::timeout(timeout, collect_limited(body, limit))
        .await
        .map_err(|_| ())?
}

async fn collect_limited<B: Body<Data = Bytes> + Unpin>(
    mut body: B,
    limit: usize,
) -> Result<Bytes, ()> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(data) = frame.into_data() {
            if data.len() > limit.saturating_sub(bytes.len()) {
                return Err(());
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(bytes))
}

async fn record<T: Into<DenialDestination>>(
    context: &RequestContext,
    destination: T,
) -> anyhow::Result<()> {
    DenialRecorder::new(context.config.deny_log.clone())
        .record(&destination.into())
        .await
}

async fn record_best_effort<T: Into<DenialDestination>>(context: &RequestContext, destination: T) {
    if let Err(error) = record(context, destination).await {
        report(&error);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use super::*;

    struct PanicOnPoll(Arc<AtomicUsize>);

    impl Body for PanicOnPoll {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("request body must not be polled")
        }
    }

    fn context(allowlist: &std::path::Path, mode: &std::path::Path) -> Arc<RequestContext> {
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .unwrap();
        Arc::new(RequestContext::new(config, PublicConnector::system()))
    }

    #[tokio::test]
    async fn denied_public_request_does_not_poll_its_body() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .uri("http://denied.example/")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(request, context(&allowlist, &mode)).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
}
