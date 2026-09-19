//! Request-head authorization and origin dispatch.
use crate::{
    config::Config,
    connect::{broker::BrokerConnector, public::PublicConnector},
    diagnostics::{DenialDestination, DenialRecorder, report},
    domain::{
        policy::{decide_local, decide_public},
        target::{LocalTarget, PublicTarget, Target, classify},
    },
    server::response::{ProxyBody, fixed, origin},
};
use anyhow::Context;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Incoming},
};
use std::{sync::Arc, time::Duration};
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) struct RequestContext {
    pub(crate) config: Config,
    pub(crate) public: PublicConnector,
    pub(crate) local: Option<BrokerConnector>,
    pub(crate) shutdown: crate::Shutdown,
}
impl RequestContext {
    pub(crate) fn new(
        config: Config,
        public: PublicConnector,
        local: Option<BrokerConnector>,
        shutdown: crate::Shutdown,
    ) -> Self {
        Self {
            config,
            public,
            local,
            shutdown,
        }
    }
}
enum HttpRoute {
    Direct(String),
    Public(PublicTarget),
    Local(LocalTarget),
    Malformed,
}
enum HeadDecision {
    Forward(HttpRoute),
    Respond(Response<ProxyBody>),
}
fn classify_http(method: &Method, uri: &hyper::Uri) -> HttpRoute {
    match classify(method, uri) {
        Target::Direct(v) => HttpRoute::Direct(v),
        Target::PublicHttp(v) => HttpRoute::Public(v),
        Target::LocalHttp(v) => HttpRoute::Local(v),
        Target::Malformed | Target::PublicConnect(_) | Target::LocalConnect(_) => {
            HttpRoute::Malformed
        }
    }
}
pub(crate) async fn handle(
    mut request: Request<Incoming>,
    context: Arc<RequestContext>,
    tunnels: Arc<tokio::sync::Mutex<tokio::task::JoinSet<anyhow::Result<()>>>>,
) -> Response<ProxyBody> {
    if request.method() == Method::CONNECT {
        return handle_connect(&mut request, context, tunnels).await;
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
                _ => fixed(StatusCode::BAD_REQUEST, None, "bad request\n"),
            }
        }
    }
}
async fn handle_connect(
    request: &mut Request<Incoming>,
    context: Arc<RequestContext>,
    tunnels: Arc<tokio::sync::Mutex<tokio::task::JoinSet<anyhow::Result<()>>>>,
) -> Response<ProxyBody> {
    match classify(request.method(), request.uri()) {
        Target::PublicConnect(target) => {
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
                    record_best_effort(&context, &target).await;
                    return fixed(StatusCode::FORBIDDEN, None, "forbidden\n");
                }
            };
            if !decision.allowed {
                record_best_effort(&context, &target).await;
                return fixed(StatusCode::FORBIDDEN, None, "forbidden\n");
            }
            if decision.record_denial
                && let Err(error) = record(&context, &target).await
            {
                report(&error);
                return fixed(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    "internal server error\n",
                );
            }
            match context.public.connect_target(target).await {
                Ok(value) => {
                    spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone()).await;
                    fixed(StatusCode::OK, None, "")
                }
                Err(error) => {
                    report(&error);
                    fixed(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
                }
            }
        }
        Target::LocalConnect(target) => {
            let allowed = match &context.config.local {
                Some(local) => {
                    decide_local(local.policy_paths.as_array(), target.canonical_authority()).await
                }
                None => Ok(false),
            };
            if !allowed.unwrap_or_else(|error| {
                report(&error);
                false
            }) {
                record_best_effort(&context, &target).await;
                return fixed(StatusCode::FORBIDDEN, None, "forbidden\n");
            }
            match &context.local {
                Some(connector) => match connector.connect(target.canonical_authority()).await {
                    Ok(value) => {
                        spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone())
                            .await;
                        fixed(StatusCode::OK, None, "")
                    }
                    Err(error) => {
                        report(&error);
                        fixed(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
                    }
                },
                None => fixed(StatusCode::FORBIDDEN, None, "forbidden\n"),
            }
        }
        _ => fixed(StatusCode::BAD_REQUEST, None, "bad request\n"),
    }
}
async fn spawn_tunnel<S>(
    request: &mut Request<Incoming>,
    upstream: S,
    shutdown: crate::Shutdown,
    tunnels: Arc<tokio::sync::Mutex<tokio::task::JoinSet<anyhow::Result<()>>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let upgrade = hyper::upgrade::on(request);
    tunnels.lock().await.spawn(async move {
        match upgrade.await {
            Ok(upgraded) => crate::server::relay::relay(
                hyper_util::rt::TokioIo::new(upgraded),
                upstream,
                shutdown,
            )
            .await
            .context("relay CONNECT tunnel"),
            Err(error) => Err(anyhow::Error::new(error).context("upgrade CONNECT request")),
        }
    });
}
async fn authorize(parts: &hyper::http::request::Parts, context: &RequestContext) -> HeadDecision {
    let route = classify_http(&parts.method, &parts.uri);
    match &route {
        HttpRoute::Direct(_) if parts.method == Method::GET => HeadDecision::Forward(route),
        HttpRoute::Direct(_) => {
            HeadDecision::Respond(fixed(StatusCode::NOT_FOUND, None, "404 page not found\n"))
        }
        HttpRoute::Malformed => HeadDecision::Forward(route),
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
        HttpRoute::Local(target) => {
            let result = match &context.config.local {
                Some(local) => {
                    decide_local(local.policy_paths.as_array(), target.canonical_authority()).await
                }
                None => Ok(false),
            };
            if !result.unwrap_or_else(|error| {
                report(&error);
                false
            }) {
                record_best_effort(context, target).await;
                return HeadDecision::Respond(fixed(StatusCode::FORBIDDEN, None, "forbidden\n"));
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
        HttpRoute::Local(target) => match &context.local {
            Some(connector) => match connector
                .http(target.canonical_authority(), target.secure(), request)
                .await
            {
                Ok(value) => origin(value),
                Err(error) => {
                    report(&error);
                    fixed(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
                }
            },
            None => fixed(StatusCode::FORBIDDEN, None, "forbidden\n"),
        },
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
    use super::*;
    use std::{
        convert::Infallible,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

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

    struct PendingBody;

    impl Body for PendingBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
            Poll::Pending
        }
    }

    fn context(allowlist: &std::path::Path, mode: &std::path::Path) -> Arc<RequestContext> {
        context_with_log(allowlist, mode, None)
    }

    fn context_with_log(
        allowlist: &std::path::Path,
        mode: &std::path::Path,
        deny_log: Option<&std::path::Path>,
    ) -> Arc<RequestContext> {
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_DENY_LOG" => deny_log.map(|path| path.display().to_string()),
            _ => None,
        })
        .unwrap();
        Arc::new(RequestContext::new(
            config,
            PublicConnector::system(),
            None,
            crate::Shutdown::new(),
        ))
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

    #[tokio::test]
    async fn healthz_does_not_poll_its_body() {
        let directory = tempfile::tempdir().unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .uri("/healthz")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(
            request,
            context(
                &directory.path().join("allowlist"),
                &directory.path().join("mode"),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_is_available_and_enforces_when_policy_is_unreadable_or_malformed() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let cases = [
            (None, Some("allowed.example\n")),
            (Some("open\n"), None),
            (Some("open\n"), Some("bad!entry\n")),
            (Some("unknown\n"), Some("allowed.example\n")),
            (Some("open\nreport\n"), Some("allowed.example\n")),
        ];
        for (mode_contents, layer_contents) in cases {
            let _ = std::fs::remove_file(&mode);
            let _ = std::fs::remove_file(&allowlist);
            if let Some(contents) = mode_contents {
                std::fs::write(&mode, contents).unwrap();
            }
            if let Some(contents) = layer_contents {
                std::fs::write(&allowlist, contents).unwrap();
            }
            let request = Request::builder()
                .uri("/__status")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = handle_http(request, context(&allowlist, &mode)).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                Bytes::from_static(b"{\"mode\":\"enforce\"}\n")
            );
        }
    }

    #[tokio::test]
    async fn collection_rejects_over_limit_frames() {
        let body = Full::new(Bytes::from_static(b"too large"));
        assert!(
            collect_with_limits(body, Duration::from_secs(1), 3)
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn collection_times_out_pending_bodies() {
        assert!(
            collect_with_limits(PendingBody, Duration::from_secs(1), 10)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn enforce_denial_stays_forbidden_when_audit_write_fails() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("deny-log-directory");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        std::fs::create_dir(&deny_log).unwrap();
        let request = Request::builder()
            .uri("http://denied.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();

        let response = handle_http(
            request,
            context_with_log(&allowlist, &mode, Some(&deny_log)),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn report_mode_audit_write_failure_is_internal_error() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("deny-log-directory");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "report\n").unwrap();
        std::fs::create_dir(&deny_log).unwrap();
        let request = Request::builder()
            .uri("http://denied.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();

        let response = handle_http(
            request,
            context_with_log(&allowlist, &mode, Some(&deny_log)),
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn non_connect_classification_never_returns_a_connect_route() {
        assert!(matches!(
            classify_http(&Method::GET, &"http://example.com/".parse().unwrap()),
            HttpRoute::Public(_)
        ));
        assert!(matches!(
            classify_http(&Method::CONNECT, &"example.com:443".parse().unwrap()),
            HttpRoute::Malformed
        ));
    }
}
