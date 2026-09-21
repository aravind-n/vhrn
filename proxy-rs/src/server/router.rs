//! Request-head authorization and origin dispatch.
use std::sync::Arc;

#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use anyhow::Context;
use bytes::Bytes;
#[cfg(test)]
use http_body_util::BodyExt;
#[cfg(test)]
use http_body_util::Full;
#[cfg(test)]
use hyper::Request;
#[cfg(test)]
use hyper::body::Body;
use hyper::{Method, Response, StatusCode, header};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};

use crate::{
    config::Config,
    connect::{
        broker::{BrokerConnector, BrokerError},
        forward::{ForwardError, ForwardErrorKind},
        public::{PublicConnectError, PublicConnector},
    },
    diagnostics::{AuditResult, AuditService, DenialDestination, Health, HealthService, report},
    domain::{
        policy::{Mode, decide_local, decide_public},
        target::{DirectTarget, LocalTarget, PublicTarget, Target, classify},
    },
    server::{
        http1::{Http1Connection, RequestHead},
        response::{ProxyBody, ProxyFailure, failure, fixed, origin, represented},
    },
};

#[cfg(test)]
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODE_BYTES: u64 = 8;
const _: () = {
    // Phase 6 supersedes the whole-policy status bridge without changing its owning module.
    let _ = crate::domain::policy::effective_status_mode;
    let _ = crate::domain::target::classify_parsed;
};
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NONBLOCK: i32 = 0x800;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const O_NONBLOCK: i32 = 0x4;
#[cfg(test)]
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
        shutdown: &crate::Shutdown,
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
        shutdown: &crate::Shutdown,
        audit: AuditService,
        health: Arc<HealthService>,
    ) -> Self {
        Self {
            config,
            public,
            local,
            shutdown: shutdown.clone(),
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

    pub(crate) fn prune_idle(&self) {
        self.public.prune_pool();
        if let Some(local) = &self.local {
            local.prune_pool();
        }
    }

    pub(crate) fn close_idle(&self) {
        self.public.close_pool();
        if let Some(local) = &self.local {
            local.close_pool();
        }
    }
}

#[cfg(test)]
use crate::domain::target::classify_parsed;
enum AuthorizedRoute {
    Direct(DirectTarget),
    Asterisk,
    PublicHttp(AuthorizedPublic),
    LocalHttp(LocalTarget),
    PublicConnect(AuthorizedPublic),
    LocalConnect(LocalTarget),
}

struct AuthorizedPublic {
    target: PublicTarget,
    effective_mode: Mode,
    already_audited: bool,
}

pub(crate) struct Http1Outcome {
    pub(crate) response: Response<ProxyBody>,
    pub(crate) body_consumed: bool,
    pub(crate) reusable: bool,
}

pub(crate) trait TunnelIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> TunnelIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(crate) type BoxTunnel = Box<dyn TunnelIo>;

pub(crate) struct ConnectedUpstream {
    pub(crate) stream: BoxTunnel,
    pub(crate) prefix: Bytes,
}

pub(crate) async fn handle_http1<S>(
    head: RequestHead,
    connection: &mut Http1Connection<S>,
    context: Arc<RequestContext>,
) -> Http1Outcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let is_head = head.method == Method::HEAD;
    let target = classify(&head.method, &head.raw_target);
    let route = match tokio::select! {
        biased;
        () = context.shutdown.cancelled() => Err(ProxyFailure::ServiceUnavailable),
        result = authorize(target, &context) => result,
    } {
        Ok(route) => route,
        Err(error) => {
            let reusable = matches!(
                error,
                ProxyFailure::PublicDenied(_) | ProxyFailure::LocalDenied(_)
            );
            return Http1Outcome {
                response: failure(error, is_head),
                body_consumed: false,
                reusable,
            };
        }
    };
    match route {
        AuthorizedRoute::Direct(target) => Http1Outcome {
            response: direct(&head.method, &target, &context).await,
            body_consumed: false,
            reusable: true,
        },
        AuthorizedRoute::Asterisk => Http1Outcome {
            response: asterisk(&head.method),
            body_consumed: false,
            reusable: head.method == Method::OPTIONS,
        },
        route @ (AuthorizedRoute::PublicHttp(_) | AuthorizedRoute::LocalHttp(_)) => {
            dispatch_http1(&head, connection, route, context).await
        }
        AuthorizedRoute::PublicConnect(_) | AuthorizedRoute::LocalConnect(_) => Http1Outcome {
            response: failure(ProxyFailure::BadRequest, is_head),
            body_consumed: false,
            reusable: false,
        },
    }
}

pub(crate) async fn connect_http1(
    head: &RequestHead,
    context: &RequestContext,
) -> Result<ConnectedUpstream, ProxyFailure> {
    let target = classify(&head.method, &head.raw_target);
    let route = tokio::select! {
        biased;
        () = context.shutdown.cancelled() => Err(ProxyFailure::ServiceUnavailable),
        result = authorize(target, context) => result,
    }?;
    match route {
        AuthorizedRoute::PublicConnect(public) => match context
            .public
            .connect_target(public.target.clone(), &context.shutdown)
            .await
        {
            Ok(stream) => Ok(ConnectedUpstream {
                stream: Box::new(stream),
                prefix: Bytes::new(),
            }),
            Err(error) => Err(public_failure(error, &public, context).await),
        },
        AuthorizedRoute::LocalConnect(target) => match &context.local {
            Some(connector) => match connector
                .connect(target.canonical_authority(), &context.shutdown)
                .await
            {
                Ok(mut stream) => {
                    let prefix = stream.take_prefix();
                    Ok(ConnectedUpstream {
                        stream: Box::new(stream),
                        prefix,
                    })
                }
                Err(error) => Err(broker_failure(error)),
            },
            None => Err(ProxyFailure::LocalDenied(
                target.canonical_authority().to_string(),
            )),
        },
        _ => Err(ProxyFailure::BadRequest),
    }
}

pub(crate) async fn drain_http1_body<S>(
    connection: &mut Http1Connection<S>,
    framing: crate::server::http1::BodyFraming,
    shutdown: &crate::Shutdown,
) -> Result<(), ()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut body = connection.incoming_body(framing);
    loop {
        let frame = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Err(()),
            frame = body.next_frame() => frame.map_err(|_| ())?,
        };
        if frame.is_none() {
            return Ok(());
        }
    }
}

#[cfg(test)]
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
                Ok(_) => match route {
                    AuthorizedRoute::PublicHttp(public) => match context
                        .public
                        .connect_target(public.target.clone(), &context.shutdown)
                        .await
                    {
                        Ok(_) => fixed(StatusCode::NO_CONTENT, None, ""),
                        Err(error) => failure(public_failure(error, &public, &context).await, head),
                    },
                    AuthorizedRoute::LocalHttp(target) => match &context.local {
                        Some(connector) => match connector
                            .connect(target.canonical_authority(), &context.shutdown)
                            .await
                        {
                            Ok(_) => fixed(StatusCode::NO_CONTENT, None, ""),
                            Err(error) => failure(broker_failure(error), head),
                        },
                        None => failure(
                            ProxyFailure::LocalDenied(target.canonical_authority().to_string()),
                            head,
                        ),
                    },
                    _ => unreachable!("matched HTTP route"),
                },
                _ => failure(ProxyFailure::BadRequest, head),
            }
        }
        Ok(AuthorizedRoute::PublicConnect(_) | AuthorizedRoute::LocalConnect(_)) => {
            failure(ProxyFailure::BadRequest, head)
        }
    }
}
#[cfg(test)]
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
        Ok(AuthorizedRoute::PublicConnect(public)) => {
            match context
                .public
                .connect_target(public.target.clone(), &context.shutdown)
                .await
            {
                Ok(value) => {
                    spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone()).await;
                    fixed(StatusCode::OK, None, "")
                }
                Err(error) => failure(public_failure(error, &public, &context).await, false),
            }
        }
        Ok(AuthorizedRoute::LocalConnect(target)) => match &context.local {
            Some(connector) => match connector
                .connect(target.canonical_authority(), &context.shutdown)
                .await
            {
                Ok(value) => {
                    spawn_tunnel(request, value, context.shutdown.clone(), tunnels.clone()).await;
                    fixed(StatusCode::OK, None, "")
                }
                Err(error) => failure(broker_failure(error), false),
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
#[cfg(test)]
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
                crate::server::relay::TunnelParts {
                    downstream: hyper_util::rt::TokioIo::new(upgraded),
                    downstream_prefix: Bytes::new(),
                    upstream,
                    upstream_prefix: Bytes::new(),
                },
                shutdown,
            )
            .await
            .map_err(anyhow::Error::new)
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
    let already_audited = decision.record_denial;
    if already_audited {
        match record(context, &target, decision.effective_mode).await {
            AuditResult::AppendFailed => {
                report(&"denial_log_append_failed");
                return Err(ProxyFailure::ReportLogUnavailable);
            }
            AuditResult::Recorded | AuditResult::Disabled => {}
        }
    }
    if connect {
        let public = AuthorizedPublic {
            target,
            effective_mode: decision.effective_mode,
            already_audited,
        };
        Ok(AuthorizedRoute::PublicConnect(public))
    } else {
        let public = AuthorizedPublic {
            target,
            effective_mode: decision.effective_mode,
            already_audited,
        };
        Ok(AuthorizedRoute::PublicHttp(public))
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
async fn dispatch_http1<S>(
    head: &RequestHead,
    connection: &mut Http1Connection<S>,
    route: AuthorizedRoute,
    context: Arc<RequestContext>,
) -> Http1Outcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let is_head = head.method == Method::HEAD;
    match route {
        AuthorizedRoute::PublicHttp(public) => match context
            .public
            .forward(public.target.clone(), head, connection, &context.shutdown)
            .await
        {
            Ok(value) => Http1Outcome {
                response: origin(value.response, is_head),
                body_consumed: value.request_complete,
                reusable: value.request_complete,
            },
            Err(crate::connect::public::PublicForwardError::Connect(error)) => Http1Outcome {
                response: failure(public_failure(error, &public, &context).await, is_head),
                body_consumed: false,
                reusable: true,
            },
            Err(crate::connect::public::PublicForwardError::Exchange(error)) => {
                forward_failure(error, is_head)
            }
        },
        AuthorizedRoute::LocalHttp(target) => match &context.local {
            Some(connector) => match connector
                .forward(&target, head, connection, &context.shutdown)
                .await
            {
                Ok(value) => Http1Outcome {
                    response: origin(value.response, is_head),
                    body_consumed: value.request_complete,
                    reusable: value.request_complete,
                },
                Err(crate::connect::broker::BrokerForwardError::Connect(error)) => Http1Outcome {
                    response: failure(broker_failure(error), is_head),
                    body_consumed: false,
                    reusable: true,
                },
                Err(crate::connect::broker::BrokerForwardError::Exchange(error)) => {
                    forward_failure(error, is_head)
                }
                Err(crate::connect::broker::BrokerForwardError::Origin(source, error)) => {
                    report(&source);
                    forward_failure(error, is_head)
                }
            },
            None => Http1Outcome {
                response: failure(
                    ProxyFailure::LocalDenied(target.canonical_authority().to_string()),
                    is_head,
                ),
                body_consumed: false,
                reusable: true,
            },
        },
        AuthorizedRoute::Direct(_)
        | AuthorizedRoute::Asterisk
        | AuthorizedRoute::PublicConnect(_)
        | AuthorizedRoute::LocalConnect(_) => Http1Outcome {
            response: failure(ProxyFailure::BadRequest, is_head),
            body_consumed: false,
            reusable: false,
        },
    }
}

fn forward_failure(error: ForwardError, head: bool) -> Http1Outcome {
    let failure_kind = match error.kind {
        ForwardErrorKind::BadRequest => ProxyFailure::BadRequest,
        ForwardErrorKind::BadGateway => ProxyFailure::BadGateway,
        ForwardErrorKind::Cancelled | ForwardErrorKind::ClientDisconnected => {
            ProxyFailure::ServiceUnavailable
        }
    };
    Http1Outcome {
        response: failure(failure_kind, head),
        body_consumed: error.request_complete,
        reusable: error.request_complete
            && !matches!(
                error.kind,
                ForwardErrorKind::Cancelled | ForwardErrorKind::ClientDisconnected
            ),
    }
}

fn broker_failure(error: BrokerError) -> ProxyFailure {
    report(&error);
    match error {
        BrokerError::DeadlineExceeded => ProxyFailure::GatewayTimeout,
        BrokerError::Cancelled | BrokerError::Exhausted => ProxyFailure::ServiceUnavailable,
        BrokerError::Rejected | BrokerError::Unavailable | BrokerError::OriginFailure => {
            ProxyFailure::BadGateway
        }
    }
}

async fn public_failure(
    error: PublicConnectError,
    public: &AuthorizedPublic,
    context: &RequestContext,
) -> ProxyFailure {
    if error.is_policy_denial() {
        if !public.already_audited {
            record_best_effort(context, &public.target, public.effective_mode).await;
        }
        ProxyFailure::PublicDenied(public.target.host().to_string())
    } else {
        report(&error);
        match error {
            PublicConnectError::DeadlineExceeded => ProxyFailure::GatewayTimeout,
            PublicConnectError::Cancelled | PublicConnectError::Exhausted => {
                ProxyFailure::ServiceUnavailable
            }
            PublicConnectError::Unavailable => ProxyFailure::BadGateway,
            PublicConnectError::PolicyDenied => unreachable!("handled above"),
        }
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
            let mode = read_status_mode(&context.config.mode_file).await;
            let status = mode.map_or(StatusCode::SERVICE_UNAVAILABLE, |_| StatusCode::OK);
            let mode = mode.unwrap_or(Mode::Enforce);
            represented(
                status,
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

async fn read_status_mode(path: &std::path::Path) -> Option<Mode> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .await
        .ok()?;
    let metadata = file.metadata().await.ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MODE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(MAX_MODE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    match bytes.as_slice() {
        b"enforce" | b"enforce\n" => Some(Mode::Enforce),
        b"report" | b"report\n" => Some(Mode::Report),
        b"open" | b"open\n" => Some(Mode::Open),
        _ => None,
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
#[cfg(test)]
async fn collect_with_limits<B: Body<Data = Bytes> + Unpin>(
    body: B,
    timeout: Duration,
    limit: usize,
) -> Result<Bytes, ()> {
    tokio::time::timeout(timeout, collect_limited(body, limit))
        .await
        .map_err(|_| ())?
}

#[cfg(test)]
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
    use crate::connect::public::{
        DialFuture, NumericDialer, ResolveFuture, ResolvedAddress, Resolver,
    };
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

    #[test]
    fn broker_failures_map_to_safe_response_classes() {
        for error in [BrokerError::Rejected, BrokerError::Unavailable] {
            assert_eq!(broker_failure(error), ProxyFailure::BadGateway);
        }
        assert_eq!(
            broker_failure(BrokerError::DeadlineExceeded),
            ProxyFailure::GatewayTimeout
        );
        assert_eq!(
            broker_failure(BrokerError::Cancelled),
            ProxyFailure::ServiceUnavailable
        );
        assert_eq!(
            broker_failure(BrokerError::Exhausted),
            ProxyFailure::ServiceUnavailable
        );
    }

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
            Box::pin(async { Ok(vec![ResolvedAddress::unscoped("8.8.8.8".parse().unwrap())]) })
        }
    }

    struct StaticResolver {
        calls: Arc<AtomicUsize>,
        answers: Vec<ResolvedAddress>,
    }

    impl Resolver for StaticResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let answers = self.answers.clone();
            Box::pin(async move { Ok(answers) })
        }
    }

    struct FailedResolver;

    impl Resolver for FailedResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            Box::pin(async { Err(anyhow::anyhow!("test resolution failure")) })
        }
    }

    struct PendingResolver;

    impl Resolver for PendingResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            Box::pin(std::future::pending())
        }
    }

    struct UnusedDialer;

    impl NumericDialer for UnusedDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            Box::pin(async { panic!("report-mode audit failure must not dial") })
        }
    }

    struct FailedDialer(Arc<AtomicUsize>);

    impl NumericDialer for FailedDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(anyhow::anyhow!("test refusal")) })
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
        context_with_public(
            allowlist,
            mode,
            deny_log,
            PublicConnector::system(crate::shutdown::ProcessResources::testing(256, 256)),
        )
    }

    fn context_with_public(
        allowlist: &std::path::Path,
        mode: &std::path::Path,
        deny_log: Option<&std::path::Path>,
        public: PublicConnector,
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
            public,
            None,
            &crate::Shutdown::new(),
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
    async fn unsafe_dns_answer_audits_once_and_never_dials() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        std::fs::write(&deny_log, "").unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let context = context_with_public(
            &allowlist,
            &mode,
            Some(&deny_log),
            PublicConnector::new(
                Arc::new(StaticResolver {
                    calls: resolves.clone(),
                    answers: vec![
                        ResolvedAddress::unscoped("8.8.8.8".parse().unwrap()),
                        ResolvedAddress::unscoped("127.0.0.1".parse().unwrap()),
                    ],
                }),
                Arc::new(UnusedDialer),
            ),
        );
        let request = Request::builder()
            .uri("http://ALLOWED.Example./")
            .body(Full::new(Bytes::new()))
            .unwrap();

        let response = handle_http(request, context).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"blocked by vhrn egress policy: allowed.example\n")
        );
        let records = std::fs::read_to_string(&deny_log).unwrap();
        assert_eq!(records.lines().count(), 1);
        assert!(records.ends_with("\tallowed.example\n"));
    }

    #[tokio::test]
    async fn unsafe_literal_audits_once_without_resolution_or_dial() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "10.0.0.1\n").unwrap();
        std::fs::write(&mode, "open\n").unwrap();
        std::fs::write(&deny_log, "").unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let context = context_with_public(
            &allowlist,
            &mode,
            Some(&deny_log),
            PublicConnector::new(
                Arc::new(StaticResolver {
                    calls: resolves.clone(),
                    answers: Vec::new(),
                }),
                Arc::new(UnusedDialer),
            ),
        );
        let request = Request::builder()
            .uri("http://10.0.0.1/")
            .body(Full::new(Bytes::new()))
            .unwrap();

        let response = handle_http(request, context).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
        let records = std::fs::read_to_string(&deny_log).unwrap();
        assert_eq!(records.lines().count(), 1);
        assert!(records.ends_with("\t10.0.0.1\n"));
    }

    #[tokio::test]
    async fn mapped_and_scoped_http_literals_are_audited_403_without_network_work() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "").unwrap();
        std::fs::write(&mode, "open\n").unwrap();

        for (uri, expected_host) in [
            ("http://[::ffff:8.8.8.8]/", "::ffff:8.8.8.8"),
            ("http://[fe80::1%25eth0]/", "fe80::1"),
        ] {
            std::fs::write(&deny_log, "").unwrap();
            let resolves = Arc::new(AtomicUsize::new(0));
            let context = context_with_public(
                &allowlist,
                &mode,
                Some(&deny_log),
                PublicConnector::new(
                    Arc::new(StaticResolver {
                        calls: resolves.clone(),
                        answers: Vec::new(),
                    }),
                    Arc::new(UnusedDialer),
                ),
            );
            let request = Request::builder()
                .uri(uri)
                .body(Full::new(Bytes::new()))
                .unwrap();

            let response = handle_http(request, context).await;

            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(resolves.load(Ordering::SeqCst), 0, "{uri}");
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                format!("blocked by vhrn egress policy: {expected_host}\n"),
                "{uri}"
            );
            let records = std::fs::read_to_string(&deny_log).unwrap();
            assert_eq!(records.lines().count(), 1, "{uri}");
            assert!(records.ends_with(&format!("\t{expected_host}\n")), "{uri}");
        }
    }

    #[tokio::test]
    async fn mapped_and_scoped_connect_literals_are_audited_403_without_network_work() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "").unwrap();
        std::fs::write(&mode, "open\n").unwrap();

        for (authority, expected_host) in [
            ("[::ffff:8.8.8.8]:443", "::ffff:8.8.8.8"),
            ("[fe80::1%25eth0]:443", "fe80::1"),
        ] {
            std::fs::write(&deny_log, "").unwrap();
            let resolves = Arc::new(AtomicUsize::new(0));
            let context = context_with_public(
                &allowlist,
                &mode,
                Some(&deny_log),
                PublicConnector::new(
                    Arc::new(StaticResolver {
                        calls: resolves.clone(),
                        answers: Vec::new(),
                    }),
                    Arc::new(UnusedDialer),
                ),
            );
            let mut request = Request::builder()
                .method(Method::CONNECT)
                .uri(authority)
                .body(Full::new(Bytes::new()))
                .unwrap();
            let tunnels = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));

            let response = handle_connect(&mut request, context, tunnels.clone()).await;

            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{authority}");
            assert_eq!(resolves.load(Ordering::SeqCst), 0, "{authority}");
            assert!(tunnels.lock().await.is_empty(), "{authority}");
            let records = std::fs::read_to_string(&deny_log).unwrap();
            assert_eq!(records.lines().count(), 1, "{authority}");
            assert!(
                records.ends_with(&format!("\t{expected_host}\n")),
                "{authority}"
            );
        }
    }

    #[tokio::test]
    async fn report_mode_address_denial_does_not_duplicate_its_prior_audit() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "other.example\n").unwrap();
        std::fs::write(&mode, "report\n").unwrap();
        std::fs::write(&deny_log, "").unwrap();
        let config = Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_DENY_LOG" => Some(deny_log.display().to_string()),
            _ => None,
        })
        .unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let authorization = Arc::new(AuthorizationSpy::default());
        let context = Arc::new(
            RequestContext::new(
                config,
                PublicConnector::new(
                    Arc::new(StaticResolver {
                        calls: resolves.clone(),
                        answers: vec![
                            ResolvedAddress::unscoped("8.8.8.8".parse().unwrap()),
                            ResolvedAddress::unscoped("127.0.0.1".parse().unwrap()),
                        ],
                    }),
                    Arc::new(UnusedDialer),
                ),
                None,
                &crate::Shutdown::new(),
            )
            .with_authorization_spy(authorization.clone()),
        );
        let request = Request::builder()
            .uri("http://allowed.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();

        let response = handle_http(request, context).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        assert_eq!(authorization.audit_writes.load(Ordering::SeqCst), 1);
        let records = std::fs::read_to_string(&deny_log).unwrap();
        assert_eq!(records.lines().count(), 1);
        assert!(records.ends_with("\tallowed.example\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn transport_failures_map_without_denial_records() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let deny_log = directory.path().join("denied");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();

        std::fs::write(&deny_log, "").unwrap();
        let response = handle_http(
            Request::builder()
                .uri("http://allowed.example/")
                .body(Full::new(Bytes::new()))
                .unwrap(),
            context_with_public(
                &allowlist,
                &mode,
                Some(&deny_log),
                PublicConnector::new(Arc::new(FailedResolver), Arc::new(UnusedDialer)),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(std::fs::read_to_string(&deny_log).unwrap().is_empty());

        std::fs::write(&deny_log, "").unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let response = handle_http(
            Request::builder()
                .uri("http://allowed.example/")
                .body(Full::new(Bytes::new()))
                .unwrap(),
            context_with_public(
                &allowlist,
                &mode,
                Some(&deny_log),
                PublicConnector::new(
                    Arc::new(StaticResolver {
                        calls: Arc::new(AtomicUsize::new(0)),
                        answers: vec![
                            ResolvedAddress::unscoped("8.8.8.8".parse().unwrap()),
                            ResolvedAddress::unscoped("1.1.1.1".parse().unwrap()),
                        ],
                    }),
                    Arc::new(FailedDialer(dials.clone())),
                ),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(dials.load(Ordering::SeqCst), 2);
        assert!(std::fs::read_to_string(&deny_log).unwrap().is_empty());

        std::fs::write(&deny_log, "").unwrap();
        let response = handle_http(
            Request::builder()
                .uri("http://allowed.example/")
                .body(Full::new(Bytes::new()))
                .unwrap(),
            context_with_public(
                &allowlist,
                &mode,
                Some(&deny_log),
                PublicConnector::new(Arc::new(PendingResolver), Arc::new(UnusedDialer)),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(std::fs::read_to_string(&deny_log).unwrap().is_empty());
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
                &crate::Shutdown::new(),
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

        let tunnels = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
        for (name, value) in [
            (header::CONTENT_LENGTH, "0"),
            (header::TRANSFER_ENCODING, "chunked"),
            (header::EXPECT, "100-continue"),
        ] {
            let mut framed_connect = Request::builder()
                .method(Method::CONNECT)
                .uri("allowed.example:443")
                .header(name, value)
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response =
                handle_connect(&mut framed_connect, context.clone(), tunnels.clone()).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

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
    async fn status_rereads_only_mode_and_fails_closed_when_mode_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let cases = [
            (
                None,
                Some("allowed.example\n"),
                StatusCode::SERVICE_UNAVAILABLE,
                "enforce",
            ),
            (Some("open\n"), None, StatusCode::OK, "open"),
            (Some("open\n"), Some("bad!entry\n"), StatusCode::OK, "open"),
            (
                Some("unknown\n"),
                Some("allowed.example\n"),
                StatusCode::SERVICE_UNAVAILABLE,
                "enforce",
            ),
            (
                Some("open\nreport\n"),
                Some("allowed.example\n"),
                StatusCode::SERVICE_UNAVAILABLE,
                "enforce",
            ),
        ];
        for (mode_contents, layer_contents, status, expected_mode) in cases {
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
            assert_eq!(response.status(), status);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                format!("{{\"mode\":\"{expected_mode}\"}}\n")
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
            &crate::Shutdown::new(),
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
            &crate::Shutdown::new(),
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
