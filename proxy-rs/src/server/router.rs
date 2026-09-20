//! Request-head authorization and origin dispatch.
use std::{sync::Arc, time::Duration};

use anyhow::Context;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Incoming},
    header,
};

use crate::{
    config::Config,
    connect::{broker::BrokerConnector, public::PublicConnector},
    diagnostics::{AuditResult, AuditService, DenialDestination, Health, HealthService, report},
    domain::{
        policy::{Mode, decide_local, decide_public},
        target::{DirectTarget, LocalTarget, PublicTarget, Target, classify_parsed},
    },
    server::response::{ProxyBody, ProxyFailure, failure, fixed, origin, represented},
};

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) struct RequestContext {
    pub(crate) config: Config,
    pub(crate) public: PublicConnector,
    pub(crate) local: Option<BrokerConnector>,
    pub(crate) shutdown: crate::Shutdown,
    audit: AuditService,
    health: Arc<HealthService>,
    #[cfg(test)]
    authorization_spy: Option<Arc<AuthorizationSpy>>,
}

#[cfg(test)]
#[derive(Default)]
struct AuthorizationSpy {
    policy_reads: std::sync::atomic::AtomicUsize,
    audit_writes: std::sync::atomic::AtomicUsize,
}

impl RequestContext {
    #[cfg(test)]
    pub(crate) fn new(
        config: Config,
        public: PublicConnector,
        local: Option<BrokerConnector>,
        shutdown: crate::Shutdown,
    ) -> Self {
        let audit = config.deny_log.clone();
        let health = Arc::new(HealthService::new(
            config.allowlists.as_slice().to_vec(),
            config.mode_file.clone(),
            config
                .local
                .as_ref()
                .map(|value| value.policy_paths.as_array().clone()),
            audit.clone(),
            shutdown.clone(),
        ));
        Self::with_services(config, public, local, shutdown, audit, health)
    }

    pub(crate) fn with_services(
        config: Config,
        public: PublicConnector,
        local: Option<BrokerConnector>,
        shutdown: crate::Shutdown,
        audit: AuditService,
        health: Arc<HealthService>,
    ) -> Self {
        Self {
            config,
            public,
            local,
            shutdown,
            audit,
            health,
            #[cfg(test)]
            authorization_spy: None,
        }
    }

    #[cfg(test)]
    fn with_authorization_spy(mut self, spy: Arc<AuthorizationSpy>) -> Self {
        self.authorization_spy = Some(spy);
        self
    }
}
enum AuthorizedRoute {
    Direct(DirectTarget),
    Asterisk,
    PublicHttp(PublicTarget),
    LocalHttp(LocalTarget),
    PublicConnect(PublicTarget),
    LocalConnect(LocalTarget),
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
    let head = parts.method == Method::HEAD;
    let target = classify_parsed(&parts.method, &parts.uri);
    match authorize(target, &context).await {
        Err(error) => failure(error, head),
        Ok(AuthorizedRoute::Direct(target)) => direct(&parts.method, &target, &context).await,
        Ok(AuthorizedRoute::Asterisk) => asterisk(&parts.method),
        Ok(route @ (AuthorizedRoute::PublicHttp(_) | AuthorizedRoute::LocalHttp(_))) => {
            match collect_with_limits(body, BODY_TIMEOUT, MAX_BODY_BYTES).await {
                Ok(body) => {
                    dispatch(
                        Request::from_parts(parts, Full::new(body)),
                        route,
                        context,
                        head,
                    )
                    .await
                }
                _ => failure(ProxyFailure::BadRequest, head),
            }
        }
        Ok(AuthorizedRoute::PublicConnect(_) | AuthorizedRoute::LocalConnect(_)) => {
            failure(ProxyFailure::BadRequest, head)
        }
    }
}
async fn handle_connect<B>(
    request: &mut Request<B>,
    context: Arc<RequestContext>,
    tunnels: Arc<tokio::sync::Mutex<tokio::task::JoinSet<anyhow::Result<()>>>>,
) -> Response<ProxyBody> {
    if request.headers().contains_key(header::TRANSFER_ENCODING)
        || request.headers().contains_key(header::EXPECT)
        || request.headers().contains_key(header::CONTENT_LENGTH)
    {
        return failure(ProxyFailure::BadRequest, false);
    }
    let target = classify_parsed(request.method(), request.uri());
    match authorize(target, &context).await {
        Ok(AuthorizedRoute::PublicConnect(target)) => {
            match context.public.connect_target(target).await {
                Ok(value) => {
                    spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone()).await;
                    fixed(StatusCode::OK, None, "")
                }
                Err(error) => {
                    report(&error);
                    failure(ProxyFailure::BadGateway, false)
                }
            }
        }
        Ok(AuthorizedRoute::LocalConnect(target)) => match &context.local {
            Some(connector) => match connector.connect(target.canonical_authority()).await {
                Ok(value) => {
                    spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone()).await;
                    fixed(StatusCode::OK, None, "")
                }
                Err(error) => {
                    report(&error);
                    failure(ProxyFailure::BadGateway, false)
                }
            },
            None => failure(
                ProxyFailure::LocalDenied(target.canonical_authority().to_string()),
                false,
            ),
        },
        Ok(_) => failure(ProxyFailure::BadRequest, false),
        Err(error) => failure(error, false),
    }
}
async fn spawn_tunnel<B, S>(
    request: &mut Request<B>,
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
async fn authorize(
    target: Target,
    context: &RequestContext,
) -> Result<AuthorizedRoute, ProxyFailure> {
    match target {
        Target::Direct(target) => Ok(AuthorizedRoute::Direct(target)),
        Target::Asterisk => Ok(AuthorizedRoute::Asterisk),
        Target::HttpsAbsoluteRejected => Err(ProxyFailure::HttpsRequiresConnect),
        Target::Malformed => Err(ProxyFailure::BadRequest),
        Target::PublicHttp(target) => authorize_public(target, false, context).await,
        Target::PublicConnect(target) => authorize_public(target, true, context).await,
        Target::LocalHttp(target) => authorize_local(target, false, context).await,
        Target::LocalConnect(target) => authorize_local(target, true, context).await,
    }
}

async fn authorize_public(
    target: PublicTarget,
    connect: bool,
    context: &RequestContext,
) -> Result<AuthorizedRoute, ProxyFailure> {
    #[cfg(test)]
    if let Some(spy) = &context.authorization_spy {
        spy.policy_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let decision = decide_public(
        context.config.allowlists.as_slice(),
        &context.config.mode_file,
        target.host(),
    )
    .await;
    let (decision, warning) = match decision {
        Ok(value) => value,
        Err(error) => {
            report(&error);
            record_best_effort(context, &target, Mode::Enforce).await;
            return Err(ProxyFailure::PublicDenied(target.host().to_string()));
        }
    };
    if let Some(warning) = warning {
        report(&warning);
    }
    if !decision.allowed {
        record_best_effort(context, &target, decision.effective_mode).await;
        return Err(ProxyFailure::PublicDenied(target.host().to_string()));
    }
    if decision.record_denial {
        match record(context, &target, decision.effective_mode).await {
            AuditResult::AppendFailed => {
                report(&"denial_log_append_failed");
                return Err(ProxyFailure::ReportLogUnavailable);
            }
            AuditResult::Recorded | AuditResult::Disabled => {}
        }
    }
    if connect {
        Ok(AuthorizedRoute::PublicConnect(target))
    } else {
        Ok(AuthorizedRoute::PublicHttp(target))
    }
}

async fn authorize_local(
    target: LocalTarget,
    connect: bool,
    context: &RequestContext,
) -> Result<AuthorizedRoute, ProxyFailure> {
    #[cfg(test)]
    if let Some(spy) = &context.authorization_spy {
        spy.policy_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let allowed = match &context.config.local {
        Some(local) => {
            match decide_local(local.policy_paths.as_array(), target.canonical_authority()).await {
                Ok(allowed) => allowed,
                Err(error) => {
                    report(&error);
                    false
                }
            }
        }
        None => false,
    };
    if !allowed {
        record_best_effort(context, &target, Mode::Enforce).await;
        return Err(ProxyFailure::LocalDenied(
            target.canonical_authority().to_string(),
        ));
    }
    if connect {
        Ok(AuthorizedRoute::LocalConnect(target))
    } else {
        Ok(AuthorizedRoute::LocalHttp(target))
    }
}
async fn dispatch(
    request: Request<Full<Bytes>>,
    route: AuthorizedRoute,
    context: Arc<RequestContext>,
    head: bool,
) -> Response<ProxyBody> {
    match route {
        AuthorizedRoute::PublicHttp(target) => match context.public.send(target, request).await {
            Ok(value) => origin(value, head),
            Err(error) => {
                report(&error);
                failure(ProxyFailure::BadGateway, head)
            }
        },
        AuthorizedRoute::LocalHttp(target) => match &context.local {
            Some(connector) => match connector.http(target.canonical_authority(), request).await {
                Ok(value) => origin(value, head),
                Err(error) => {
                    report(&error);
                    failure(ProxyFailure::BadGateway, head)
                }
            },
            None => failure(
                ProxyFailure::LocalDenied(target.canonical_authority().to_string()),
                head,
            ),
        },
        AuthorizedRoute::Direct(_)
        | AuthorizedRoute::Asterisk
        | AuthorizedRoute::PublicConnect(_)
        | AuthorizedRoute::LocalConnect(_) => failure(ProxyFailure::BadRequest, head),
    }
}
async fn direct(
    method: &Method,
    target: &DirectTarget,
    context: &RequestContext,
) -> Response<ProxyBody> {
    let head = method == Method::HEAD;
    let path = target.path_and_query();
    if matches!(path, b"/healthz" | b"/__status") && method != Method::GET && !head {
        let mut response = represented(
            StatusCode::METHOD_NOT_ALLOWED,
            Some("text/plain; charset=utf-8"),
            "method not allowed\n",
            false,
        );
        response
            .headers_mut()
            .insert(header::ALLOW, header::HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    match path {
        b"/healthz" => match context.health.read().await {
            Health::Healthy => represented(
                StatusCode::OK,
                Some("text/plain; charset=utf-8"),
                "ok\n",
                head,
            ),
            Health::Unhealthy => represented(
                StatusCode::SERVICE_UNAVAILABLE,
                Some("text/plain; charset=utf-8"),
                "unhealthy\n",
                head,
            ),
        },
        b"/__status" => {
            let (mode, warning) = crate::domain::policy::effective_status_mode(
                context.config.allowlists.as_slice(),
                &context.config.mode_file,
            )
            .await;
            if let Some(warning) = warning {
                report(&warning);
            }
            represented(
                StatusCode::OK,
                Some("application/json"),
                &format!("{{\"mode\":\"{mode}\"}}\n"),
                head,
            )
        }
        _ => represented(
            StatusCode::NOT_FOUND,
            Some("text/plain; charset=utf-8"),
            "404 page not found\n",
            head,
        ),
    }
}

fn asterisk(method: &Method) -> Response<ProxyBody> {
    if method != Method::OPTIONS {
        return failure(ProxyFailure::BadRequest, method == Method::HEAD);
    }
    let mut response = fixed(StatusCode::NO_CONTENT, None, "");
    response.headers_mut().insert(
        header::ALLOW,
        header::HeaderValue::from_static("GET, HEAD, OPTIONS, CONNECT"),
    );
    response
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
    mode: Mode,
) -> AuditResult {
    #[cfg(test)]
    if let Some(spy) = &context.authorization_spy {
        spy.audit_writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    context.audit.record(&destination.into(), mode).await
}
async fn record_best_effort<T: Into<DenialDestination>>(
    context: &RequestContext,
    destination: T,
    mode: Mode,
) {
    if record(context, destination, mode).await == AuditResult::AppendFailed {
        report(&"denial_log_append_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::public::{DialFuture, NumericDialer, ResolveFuture, Resolver};
    use std::{
        convert::Infallible,
        net::SocketAddr,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    use tokio::io::AsyncReadExt;

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

    struct CountingResolver(Arc<AtomicUsize>);

    impl Resolver for CountingResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
        }
    }

    struct UnusedDialer;

    impl NumericDialer for UnusedDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            Box::pin(async { panic!("report-mode audit failure must not dial") })
        }
    }

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
            .header(header::HOST, "allowed.example")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(request, context(&allowlist, &mode)).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"blocked by vhrn egress policy: denied.example\n")
        );
    }

    #[tokio::test]
    async fn malformed_and_https_targets_have_no_authorization_or_network_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("missing-allowlist");
        let mode = directory.path().join("missing-mode");
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let authorization = Arc::new(AuthorizationSpy::default());
        let (broker_stream, mut broker_peer) = tokio::io::duplex(1024);
        let context = Arc::new(
            RequestContext::new(
                config,
                PublicConnector::new(
                    Arc::new(CountingResolver(resolves.clone())),
                    Arc::new(UnusedDialer),
                ),
                Some(BrokerConnector::test_with_connect_stream(broker_stream)),
                crate::Shutdown::new(),
            )
            .with_authorization_spy(authorization.clone()),
        );

        for (uri, status, expected) in [
            (
                "https://allowed.example/path",
                StatusCode::BAD_REQUEST,
                Bytes::from_static(b"HTTPS requires CONNECT\n"),
            ),
            (
                "ftp://allowed.example/path",
                StatusCode::BAD_REQUEST,
                Bytes::from_static(b"bad request\n"),
            ),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let request = Request::builder()
                .uri(uri)
                .body(PanicOnPoll(polls.clone()))
                .unwrap();
            let response = handle_http(request, context.clone()).await;
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(response.headers()[header::CONNECTION], "close");
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                expected,
                "{uri}"
            );
            assert_eq!(polls.load(Ordering::SeqCst), 0, "{uri}");
        }

        let mut framed_connect = Request::builder()
            .method(Method::CONNECT)
            .uri("allowed.example:443")
            .header(header::CONTENT_LENGTH, "0")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let tunnels = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
        let response = handle_connect(&mut framed_connect, context.clone(), tunnels).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        assert_eq!(authorization.policy_reads.load(Ordering::SeqCst), 0);
        assert_eq!(authorization.audit_writes.load(Ordering::SeqCst), 0);
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
        assert_eq!(context.public.public_calls(), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), broker_peer.read_u8())
                .await
                .is_err(),
            "broker spy must observe no frame"
        );
    }

    #[tokio::test]
    async fn invalid_public_policy_is_an_exact_enforced_denial_before_body_polling() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "bad!policy\n").unwrap();
        std::fs::write(&mode, "open\n").unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .uri("http://allowed.example/")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(request, context(&allowlist, &mode)).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"blocked by vhrn egress policy: allowed.example\n")
        );
    }

    #[tokio::test]
    async fn local_denial_is_exact_and_does_not_poll_the_body() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "").unwrap();
        std::fs::write(&mode, "open\n").unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .uri("http://LOCALHOST:0080/")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(request, context(&allowlist, &mode)).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"blocked by vhrn local policy: localhost:80\n")
        );
    }

    #[tokio::test]
    async fn healthz_does_not_poll_its_body() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .uri("/healthz")
            .body(PanicOnPoll(polls.clone()))
            .unwrap();

        let response = handle_http(request, context(&allowlist, &mode)).await;

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
    async fn report_mode_audit_write_failure_is_503_and_never_dials() {
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
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_DENY_LOG" => Some(deny_log.display().to_string()),
            _ => None,
        })
        .unwrap();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let context = Arc::new(RequestContext::new(
            config,
            PublicConnector::test_with_stream(upstream),
            None,
            crate::Shutdown::new(),
        ));

        let response = handle_http(request, context).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"proxy temporarily unavailable\n")
        );
        let mut dial_bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut dial_bytes))
            .await
            .expect("connector closes without a dial")
            .expect("read connector peer");
        assert!(dial_bytes.is_empty(), "report append failure must not dial");
    }

    #[tokio::test]
    async fn report_mode_connect_audit_write_failure_is_503_and_never_resolves() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("deny-log-directory");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "report\n").unwrap();
        std::fs::create_dir(&deny_log).unwrap();
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_DENY_LOG" => Some(deny_log.display().to_string()),
            _ => None,
        })
        .unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let context = Arc::new(RequestContext::new(
            config,
            PublicConnector::new(
                Arc::new(CountingResolver(resolves.clone())),
                Arc::new(UnusedDialer),
            ),
            None,
            crate::Shutdown::new(),
        ));
        let mut request = Request::builder()
            .method(Method::CONNECT)
            .uri("denied.example:443")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let tunnels = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));

        let response = handle_connect(&mut request, context, tunnels.clone()).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"proxy temporarily unavailable\n")
        );
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
        assert!(tunnels.lock().await.is_empty());
    }

    #[test]
    fn parsed_component_bridge_keeps_http_and_connect_disjoint() {
        assert!(matches!(
            classify_parsed(&Method::GET, &"http://example.com/".parse().unwrap()),
            Target::PublicHttp(_)
        ));
        assert!(matches!(
            classify_parsed(&Method::CONNECT, &"example.com:443".parse().unwrap()),
            Target::PublicConnect(_)
        ));
    }
}
