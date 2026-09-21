//! Listener startup and HTTP connection serving.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use hyper::{Method, Version, header};
use tokio::net::TcpListener;

use crate::{
    Bootstrap, Shutdown,
    server::{
        http1::{Http1Connection, IngressError, RequestHead},
        relay::TunnelParts,
        response::{
            ProxyFailure, failure, write_connect_established_drain_aware, write_response,
            write_response_drain_aware, write_response_prompt,
        },
        router::{BoxTunnel, RequestContext, connect_http1, drain_http1_body, handle_http1},
    },
    shutdown::ProcessResources,
};

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const OVERLOAD_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 20\r\n\r\nservice unavailable\n";

pub(crate) async fn bind(address: std::net::SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(address).await.map_err(|error| {
        let category = match error.kind() {
            std::io::ErrorKind::AddrInUse => "address_in_use",
            std::io::ErrorKind::AddrNotAvailable => "address_unavailable",
            std::io::ErrorKind::PermissionDenied => "permission_denied",
            _ => "io_error",
        };
        anyhow!("startup_listener_bind_failed:{category}")
    })
}

/// Serves accepted connections until shutdown or an accept failure drains them.
pub(crate) async fn serve(bootstrap: Bootstrap) -> Result<()> {
    let Bootstrap {
        listener,
        config,
        audit,
        health,
        public,
        local,
        resources,
        shutdown,
    } = bootstrap;
    let context = Arc::new(RequestContext::with_services(
        config, public, local, &shutdown, audit, health,
    ));
    Supervisor::new(listener, context, resources, shutdown)
        .run()
        .await
}

struct Supervisor {
    listener: Option<TcpListener>,
    context: Arc<RequestContext>,
    resources: ProcessResources,
    shutdown: Shutdown,
    clients: tokio::task::JoinSet<anyhow::Result<()>>,
}

impl Supervisor {
    fn new(
        listener: TcpListener,
        context: Arc<RequestContext>,
        resources: ProcessResources,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            listener: Some(listener),
            context,
            resources,
            shutdown,
            clients: tokio::task::JoinSet::new(),
        }
    }

    async fn run(mut self) -> Result<()> {
        let mut sweep = tokio::time::interval(POOL_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut outcome = loop {
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => break Ok(()),
                joined = self.clients.join_next(), if !self.clients.is_empty() => {
                    if let Some(joined) = joined {
                        match report_task_result(joined, false, "join proxy connection") {
                            TaskOutcome::Completed => {}
                            TaskOutcome::Failed | TaskOutcome::JoinFailed | TaskOutcome::Aborted => {
                                break Err(anyhow!("supervisor_client_task_failed"));
                            }
                        }
                    }
                }
                _ = sweep.tick() => self.context.prune_idle(),
                accepted = self.listener.as_ref().expect("listener is present").accept() => match accepted {
                    Ok((stream, _)) => {
                        match self.resources.try_admit_client() {
                            Some(lease) => {
                                let context = self.context.clone();
                                let shutdown = self.shutdown.clone();
                                self.clients.spawn(async move {
                                    let _lease = lease;
                                    serve_client(stream, context, shutdown).await;
                                    Ok(())
                                });
                            }
                            None => reject_overload(&stream),
                        }
                    }
                    Err(_) => break Err(anyhow!("listener_accept_failed")),
                },
            }
        };

        self.listener.take();
        self.shutdown.request();
        self.context.close_idle();
        let deadline = self.shutdown.deadline(GRACEFUL_SHUTDOWN_TIMEOUT);
        let summary = drain_tasks(
            &mut self.clients,
            deadline,
            &self.shutdown,
            &self.resources,
            "join proxy connection",
        )
        .await;
        if summary.failed != 0 || summary.join_failed != 0 {
            outcome = Err(anyhow!("supervisor_client_task_failed"));
        }
        if self.resources.registered_sockets() != 0 {
            self.shutdown.force();
            outcome = Err(anyhow!("supervisor_socket_cleanup_failed"));
        }
        outcome
    }
}

fn reject_overload(stream: &tokio::net::TcpStream) {
    let _ = stream.try_write(OVERLOAD_RESPONSE);
}

async fn serve_client<S>(stream: S, context: Arc<RequestContext>, shutdown: Shutdown)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    match serve_connection_io(stream, context, shutdown.clone()).await {
        Ok(ConnectionOutcome::Complete) => {}
        Ok(ConnectionOutcome::Tunnel(parts)) => {
            if let Err(error) = crate::server::relay::relay(parts, shutdown).await {
                crate::diagnostics::report(&error);
            }
        }
        Err(error) => crate::diagnostics::report(&error),
    }
}
#[cfg(test)]
pub(crate) async fn serve_test_connection<S>(
    stream: S,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    match serve_connection_io(stream, context, shutdown.clone()).await? {
        ConnectionOutcome::Complete => Ok(()),
        ConnectionOutcome::Tunnel(parts) => crate::server::relay::relay(parts, shutdown)
            .await
            .map_err(anyhow::Error::new),
    }
}

enum ConnectionOutcome<S> {
    Complete,
    Tunnel(TunnelParts<S, BoxTunnel>),
}

async fn serve_connection_io<S>(
    stream: S,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<ConnectionOutcome<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    if shutdown.is_requested() {
        return Ok(ConnectionOutcome::Complete);
    }
    let mut connection = Http1Connection::new(stream);
    loop {
        let Some(head) = read_request_head(&mut connection, &shutdown).await else {
            return Ok(ConnectionOutcome::Complete);
        };

        if head.method == Method::CONNECT {
            let framed = head.headers.contains_key(header::TRANSFER_ENCODING)
                || head.headers.contains_key(header::CONTENT_LENGTH)
                || head.expect;
            if framed {
                let response = failure(ProxyFailure::BadRequest, false);
                let _ = write_response(&mut connection, response, head.version, &head.method, true)
                    .await;
                return Ok(ConnectionOutcome::Complete);
            }
            let upstream = match tokio::select! {
                biased;
                _ = connection.wait_for_peer_close() => {
                    return Ok(ConnectionOutcome::Complete);
                }
                result = connect_http1(&head, &context) => result,
            } {
                Ok(upstream) => upstream,
                Err(error) => {
                    let response = failure(error, false);
                    let close = head.close;
                    let closed = write_response(
                        &mut connection,
                        response,
                        head.version,
                        &head.method,
                        close,
                    )
                    .await?;
                    if closed {
                        return Ok(ConnectionOutcome::Complete);
                    }
                    continue;
                }
            };
            if shutdown.is_requested() {
                let response = failure(ProxyFailure::ServiceUnavailable, false);
                let _ = write_response(&mut connection, response, head.version, &head.method, true)
                    .await;
                return Ok(ConnectionOutcome::Complete);
            }
            if !write_connect_established_drain_aware(&mut connection, &shutdown).await? {
                return Ok(ConnectionOutcome::Complete);
            }
            let (downstream, downstream_prefix) = connection.into_buffered_io().into_parts();
            return Ok(ConnectionOutcome::Tunnel(TunnelParts {
                downstream,
                downstream_prefix,
                upstream: upstream.stream,
                upstream_prefix: upstream.prefix,
            }));
        }

        if serve_http_request(head, &mut connection, context.clone(), &shutdown).await? {
            return Ok(ConnectionOutcome::Complete);
        }
    }
}

async fn serve_http_request<S>(
    head: RequestHead,
    connection: &mut Http1Connection<S>,
    context: Arc<RequestContext>,
    shutdown: &Shutdown,
) -> Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if head.upgrade {
        let response = failure(ProxyFailure::NotImplemented, head.method == Method::HEAD);
        let _ = write_response(connection, response, head.version, &head.method, true).await;
        return Ok(true);
    }

    let framing = head.framing;
    let request_expect = head.expect;
    let request_close = head.close;
    let version = head.version;
    let method = head.method.clone();
    let outcome = handle_http1(head, connection, context).await;
    if shutdown.is_requested() {
        let response = failure(ProxyFailure::ServiceUnavailable, method == Method::HEAD);
        let _ = write_response_prompt(connection, response, version, &method, true, shutdown).await;
        return Ok(true);
    }
    let close = request_close
        || !outcome.reusable
        || (!outcome.body_consumed && request_expect && framing.has_body());
    let Some(closed) = write_response_drain_aware(
        connection,
        outcome.response,
        version,
        &method,
        close,
        shutdown,
    )
    .await?
    else {
        return Ok(true);
    };
    if closed || shutdown.is_requested() {
        return Ok(true);
    }
    if !outcome.body_consumed
        && framing.has_body()
        && drain_http1_body(connection, framing, shutdown)
            .await
            .is_err()
    {
        return Ok(true);
    }
    Ok(false)
}

async fn read_request_head<S>(
    connection: &mut Http1Connection<S>,
    shutdown: &Shutdown,
) -> Option<RequestHead>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let started = tokio::select! {
        biased;
        () = shutdown.cancelled() => return None,
        result = connection.wait_for_head_start() => result,
    };
    if !matches!(started, Ok(true)) {
        return None;
    }
    let result = tokio::select! {
        biased;
        () = shutdown.cancelled() => return None,
        result = tokio::time::timeout(REQUEST_HEAD_TIMEOUT, connection.read_head()) => result,
    };
    match result {
        Ok(Ok(Some(head))) => Some(head),
        Err(_) | Ok(Ok(None)) => None,
        Ok(Err(error)) => {
            let is_head = connection.request_is_head();
            let failure_kind = match error {
                IngressError::HeadersTooLarge => ProxyFailure::HeadersTooLarge,
                IngressError::UnsupportedVersion => ProxyFailure::UnsupportedVersion,
                IngressError::BadRequest | IngressError::Incomplete => ProxyFailure::BadRequest,
            };
            let response = failure(failure_kind, is_head);
            let method = if is_head { Method::HEAD } else { Method::GET };
            let _ = write_response(connection, response, Version::HTTP_11, &method, true).await;
            None
        }
    }
}

async fn drain_tasks(
    tasks: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    deadline: tokio::time::Instant,
    shutdown: &Shutdown,
    resources: &ProcessResources,
    join_context: &'static str,
) -> DrainSummary {
    let mut summary = DrainSummary::default();
    while !tasks.is_empty() {
        let joined = tokio::select! {
            biased;
            () = shutdown.forced() => break,
            () = tokio::time::sleep_until(deadline) => break,
            joined = tasks.join_next() => match joined {
                Some(joined) => joined,
                None => return summary,
            },
        };
        summary.record(report_task_result(joined, false, join_context));
    }
    if tasks.is_empty() {
        return summary;
    }
    resources.force_close_all();
    shutdown.force();
    tokio::task::yield_now().await;
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        summary.record(report_task_result(joined, true, join_context));
    }
    summary
}

#[derive(Default)]
struct DrainSummary {
    completed: usize,
    failed: usize,
    join_failed: usize,
    aborted: usize,
}

impl DrainSummary {
    fn record(&mut self, outcome: TaskOutcome) {
        match outcome {
            TaskOutcome::Completed => self.completed += 1,
            TaskOutcome::Failed => self.failed += 1,
            TaskOutcome::JoinFailed => self.join_failed += 1,
            TaskOutcome::Aborted => self.aborted += 1,
        }
    }
}

#[derive(Clone, Copy)]
enum TaskOutcome {
    Completed,
    Failed,
    JoinFailed,
    Aborted,
}

fn report_task_result(
    joined: Result<anyhow::Result<()>, tokio::task::JoinError>,
    aborted: bool,
    join_context: &'static str,
) -> TaskOutcome {
    match joined {
        Ok(Ok(())) => TaskOutcome::Completed,
        Ok(Err(error)) => {
            crate::diagnostics::report(&error);
            TaskOutcome::Failed
        }
        Err(error) if aborted && error.is_cancelled() => TaskOutcome::Aborted,
        Err(error) => {
            crate::diagnostics::report(&anyhow::Error::new(error).context(join_context));
            TaskOutcome::JoinFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::pending,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context as TaskContext, Poll},
    };
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
        sync::Notify,
    };

    struct PendingTask(Arc<AtomicBool>);

    impl Drop for PendingTask {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct BlockedWriteIo {
        request: Vec<u8>,
        read: usize,
        write_polled: Arc<Notify>,
    }

    impl BlockedWriteIo {
        fn new(request: &[u8]) -> (Self, Arc<Notify>) {
            let write_polled = Arc::new(Notify::new());
            (
                Self {
                    request: request.to_vec(),
                    read: 0,
                    write_polled: write_polled.clone(),
                },
                write_polled,
            )
        }
    }

    impl AsyncRead for BlockedWriteIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.read == this.request.len() {
                return Poll::Pending;
            }
            let count = buffer
                .remaining()
                .min(this.request.len().saturating_sub(this.read));
            buffer.put_slice(&this.request[this.read..this.read + count]);
            this.read += count;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for BlockedWriteIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            _: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.write_polled.notify_one();
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn active_task_can_finish_at_four_point_nine_nine_nine_seconds() {
        let shutdown = Shutdown::new();
        let started = tokio::time::Instant::now();
        shutdown.request();
        let resources = ProcessResources::testing(8, 8);
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async {
            tokio::time::sleep(Duration::from_millis(4_999)).await;
            Ok(())
        });

        let summary = drain_tasks(
            &mut tasks,
            shutdown.deadline(GRACEFUL_SHUTDOWN_TIMEOUT),
            &shutdown,
            &resources,
            "join active task",
        )
        .await;

        assert_eq!(started.elapsed(), Duration::from_millis(4_999));
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.aborted, 0);
        assert!(!shutdown.is_forced());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_task_is_forced_and_observed_at_exactly_five_seconds() {
        let dropped = Arc::new(AtomicBool::new(false));
        let shutdown = Shutdown::new();
        let started = tokio::time::Instant::now();
        shutdown.request();
        tokio::time::advance(Duration::from_secs(2)).await;
        shutdown.request();
        let resources = ProcessResources::testing(8, 8);
        let mut tasks = tokio::task::JoinSet::new();
        let pending_dropped = dropped.clone();
        let tracked_socket = resources.try_admit_client().expect("tracked client socket");
        tasks.spawn(async move {
            let _guard = PendingTask(pending_dropped);
            let _socket = tracked_socket;
            pending::<()>().await;
            Ok(())
        });
        assert_eq!(resources.registered_sockets(), 1);

        let summary = drain_tasks(
            &mut tasks,
            shutdown.deadline(GRACEFUL_SHUTDOWN_TIMEOUT),
            &shutdown,
            &resources,
            "join pending task",
        )
        .await;

        assert_eq!(started.elapsed(), GRACEFUL_SHUTDOWN_TIMEOUT);
        assert_eq!(summary.aborted, 1);
        assert!(shutdown.is_forced());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(resources.registered_sockets(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_tunnel_is_closed_at_the_five_second_force_deadline() {
        let (downstream, mut client) = tokio::io::duplex(64);
        let (upstream, mut origin) = tokio::io::duplex(64);
        let shutdown = Shutdown::new();
        let started = tokio::time::Instant::now();
        shutdown.request();
        let resources = ProcessResources::testing(8, 8);
        let mut tasks = tokio::task::JoinSet::new();
        let task_shutdown = shutdown.clone();
        tasks.spawn(async move {
            crate::server::relay::relay(
                TunnelParts {
                    downstream,
                    downstream_prefix: bytes::Bytes::new(),
                    upstream,
                    upstream_prefix: bytes::Bytes::new(),
                },
                task_shutdown,
            )
            .await
            .map_err(anyhow::Error::new)
        });

        let summary = drain_tasks(
            &mut tasks,
            shutdown.deadline(GRACEFUL_SHUTDOWN_TIMEOUT),
            &shutdown,
            &resources,
            "join idle tunnel",
        )
        .await;

        assert_eq!(started.elapsed(), GRACEFUL_SHUTDOWN_TIMEOUT);
        assert_eq!(summary.completed + summary.aborted, 1);
        let mut byte = [0_u8; 1];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
        assert_eq!(origin.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn second_signal_forces_idempotent_cleanup_without_waiting_for_deadline() {
        let shutdown = Shutdown::new();
        shutdown.request();
        shutdown.request();
        shutdown.force();
        shutdown.force();
        let started = tokio::time::Instant::now();
        let resources = ProcessResources::testing(8, 8);
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async {
            pending::<()>().await;
            Ok(())
        });

        let summary = drain_tasks(
            &mut tasks,
            shutdown.deadline(GRACEFUL_SHUTDOWN_TIMEOUT),
            &shutdown,
            &resources,
            "join forced task",
        )
        .await;

        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(summary.aborted, 1);
        assert!(tasks.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn drain_observes_every_tunnel_result_and_aborts_pending_tasks() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut tunnels = tokio::task::JoinSet::new();
        tunnels.spawn(async { Ok(()) });
        tunnels.spawn(async { Err(anyhow::anyhow!("relay failed")) });
        tunnels.spawn(async { panic!("relay panicked") });
        let pending_dropped = dropped.clone();
        tunnels.spawn(async move {
            let _guard = PendingTask(pending_dropped);
            pending::<()>().await;
            Ok(())
        });

        let shutdown = Shutdown::new();
        let resources = ProcessResources::testing(8, 8);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let summary = drain_tasks(
            &mut tunnels,
            deadline,
            &shutdown,
            &resources,
            "join test tunnel",
        )
        .await;

        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.join_failed, 1);
        assert_eq!(summary.aborted, 1);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(tunnels.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn connection_drain_observes_every_result_and_aborts_pending_tasks() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut connections = tokio::task::JoinSet::new();
        connections.spawn(async { Ok(()) });
        connections.spawn(async { Err(anyhow::anyhow!("connection failed")) });
        connections.spawn(async { panic!("connection panicked") });
        let pending_dropped = dropped.clone();
        connections.spawn(async move {
            let _guard = PendingTask(pending_dropped);
            pending::<()>().await;
            Ok(())
        });

        let shutdown = Shutdown::new();
        let resources = ProcessResources::testing(8, 8);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let summary = drain_tasks(
            &mut connections,
            deadline,
            &shutdown,
            &resources,
            "join test connection",
        )
        .await;

        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.join_failed, 1);
        assert_eq!(summary.aborted, 1);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(connections.is_empty());
    }

    #[test]
    fn production_admission_rejects_the_two_hundred_fifty_seventh_connection() {
        let resources = ProcessResources::testing(
            crate::shutdown::MAX_CLIENT_CONNECTIONS,
            crate::shutdown::MAX_UPSTREAM_CONNECTIONS,
        );
        let connections: Vec<_> = (0..crate::shutdown::MAX_CLIENT_CONNECTIONS)
            .map(|_| resources.try_admit_client().expect("client permit"))
            .collect();

        assert!(resources.try_admit_client().is_none());
        assert_eq!(
            resources.client_counts(),
            (
                crate::shutdown::MAX_CLIENT_CONNECTIONS,
                crate::shutdown::MAX_CLIENT_CONNECTIONS
            )
        );
        drop(connections);
        assert!(resources.try_admit_client().is_some());
    }

    #[test]
    fn overload_response_is_prompt_bounded_503_with_connection_close() {
        assert!(OVERLOAD_RESPONSE.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(
            OVERLOAD_RESPONSE
                .windows(b"Connection: close\r\n".len())
                .any(|value| value == b"Connection: close\r\n")
        );
        assert!(OVERLOAD_RESPONSE.len() <= 1024);
    }

    #[tokio::test]
    async fn serving_path_rejects_client_overload_without_another_task() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let context = direct_context(directory.path(), &shutdown);
        let resources = ProcessResources::testing(1, 8);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let supervisor_shutdown = shutdown.clone();
        let supervisor_resources = resources.clone();
        let supervisor = tokio::spawn(
            Supervisor::new(listener, context, resources.clone(), supervisor_shutdown).run(),
        );

        let mut admitted = tokio::net::TcpStream::connect(address).await.unwrap();
        while resources.client_counts().0 != 1 {
            tokio::task::yield_now().await;
        }
        let mut rejected = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        rejected.read_to_end(&mut response).await.unwrap();

        assert!(response.is_empty() || response == OVERLOAD_RESPONSE);
        assert_eq!(resources.client_counts(), (1, 1));
        shutdown.request();
        let mut end = [0_u8; 1];
        assert_eq!(admitted.read(&mut end).await.unwrap(), 0);
        supervisor
            .await
            .expect("supervisor task")
            .expect("graceful supervisor result");
        assert_eq!(supervisor_resources.client_counts().0, 0);
    }

    #[tokio::test]
    async fn supervisor_task_panic_cleans_up_and_returns_failure() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let context = direct_context(directory.path(), &shutdown);
        let resources = ProcessResources::testing(1, 8);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut supervisor =
            Supervisor::new(listener, context, resources.clone(), shutdown.clone());
        supervisor.clients.spawn(async {
            panic!("injected client task panic");
        });

        let error = supervisor.run().await.expect_err("supervisor failure");

        assert_eq!(error.to_string(), "supervisor_client_task_failed");
        assert!(shutdown.is_requested());
        assert_eq!(resources.registered_sockets(), 0);
    }

    fn direct_context(directory: &std::path::Path, shutdown: &Shutdown) -> Arc<RequestContext> {
        let allowlist = directory.join("allowlist");
        let mode = directory.join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::system(
                crate::shutdown::ProcessResources::testing(256, 256),
            ),
            None,
            shutdown,
        ))
    }

    #[tokio::test]
    async fn draining_before_connect_response_commit_emits_no_success() {
        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let shutdown = Shutdown::new();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::test_with_stream(upstream),
            None,
            &shutdown,
        ));
        let (stream, write_polled) = BlockedWriteIo::new(
            b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n",
        );
        let task_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(stream, context, task_shutdown));

        write_polled.notified().await;
        shutdown.request();
        server.await.expect("server task").expect("server result");

        let mut end = [0_u8; 1];
        assert_eq!(peer.read(&mut end).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn draining_before_http_response_commit_emits_no_origin_response() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let context = direct_context(directory.path(), &shutdown);
        let (stream, write_polled) = BlockedWriteIo::new(
            b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        );
        let task_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(stream, context, task_shutdown));

        write_polled.notified().await;
        shutdown.request();
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn writable_http_race_emits_closing_503_during_drain() {
        struct PendingResolver(Arc<Notify>);
        impl crate::connect::public::Resolver for PendingResolver {
            fn resolve(&self, _: String, _: u16) -> crate::connect::public::ResolveFuture {
                let entered = self.0.clone();
                Box::pin(async move {
                    entered.notify_one();
                    pending().await
                })
            }
        }
        struct UnusedDialer;
        impl crate::connect::public::NumericDialer for UnusedDialer {
            fn dial(&self, _: std::net::SocketAddr) -> crate::connect::public::DialFuture {
                panic!("pending resolution must not dial")
            }
        }

        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let entered = Arc::new(Notify::new());
        let shutdown = Shutdown::new();
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::new(
                Arc::new(PendingResolver(entered.clone())),
                Arc::new(UnusedDialer),
            ),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(2048);
        let task_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(server_stream, context, task_shutdown));
        client
            .write_all(b"GET http://allowed.example/ HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .expect("proxy request");

        entered.notified().await;
        shutdown.request();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).expect("response text");

        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(
            response
                .to_ascii_lowercase()
                .contains("connection: close\r\n")
        );
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn upstream_exhaustion_is_visible_as_closing_http_503() {
        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let shutdown = Shutdown::new();
        let resources = ProcessResources::testing(8, 1);
        let held = resources.manage_upstream((), resources.try_upstream().unwrap());
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::system(resources.clone()),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(2048);
        let server = tokio::spawn(serve_test_connection(server_stream, context, shutdown));
        client
            .write_all(b"GET http://allowed.example/ HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .expect("proxy request");
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert!(response.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(
            response
                .windows(b"Connection: close\r\n".len())
                .any(|value| { value.eq_ignore_ascii_case(b"Connection: close\r\n") })
        );
        assert_eq!(resources.upstream_counts(), (1, 1));
        server.await.expect("server task").expect("server result");
        drop(held);
        assert!(resources.try_upstream().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn public_connect_emits_no_success_while_resolution_is_pending() {
        struct PendingResolver;
        impl crate::connect::public::Resolver for PendingResolver {
            fn resolve(&self, _: String, _: u16) -> crate::connect::public::ResolveFuture {
                Box::pin(pending())
            }
        }
        struct UnusedDialer;
        impl crate::connect::public::NumericDialer for UnusedDialer {
            fn dial(&self, _: std::net::SocketAddr) -> crate::connect::public::DialFuture {
                panic!("pending resolution must not dial")
            }
        }

        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let shutdown = Shutdown::new();
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::new(
                Arc::new(PendingResolver),
                Arc::new(UnusedDialer),
            ),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(1024);
        let task_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(server_stream, context, task_shutdown));
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n")
            .await
            .expect("CONNECT request");

        assert!(
            tokio::time::timeout(Duration::from_secs(1), client.read_u8())
                .await
                .is_err(),
            "CONNECT must not commit success before resolution and dial"
        );

        shutdown.request();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 503"));
        assert!(!response.windows(7).any(|value| value == b"200 Con"));
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(start_paused = true)]
    async fn active_http_exchange_completes_at_four_point_nine_nine_nine_seconds() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let (origin_stream, mut origin) = tokio::io::duplex(4096);
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::test_with_stream(origin_stream),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(4096);
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(
            server_stream,
            context,
            server_shutdown,
        ));
        let origin_task = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(origin.read_u8().await.expect("origin request"));
            }
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\no")
                .await
                .expect("origin response prefix");
            tokio::time::sleep(Duration::from_millis(4_999)).await;
            origin.write_all(b"k").await.expect("origin response end");
            origin.shutdown().await.expect("origin shutdown");
        });

        client
            .write_all(b"GET http://allowed.example/ HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .expect("proxy request");
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\no") {
            response.push(client.read_u8().await.expect("response prefix"));
        }
        let drain_started = tokio::time::Instant::now();
        shutdown.request();
        client
            .read_to_end(&mut response)
            .await
            .expect("drained response");

        assert_eq!(drain_started.elapsed(), Duration::from_millis(4_999));
        assert!(response.ends_with(b"\r\n\r\nok"));
        server.await.expect("server task").expect("server result");
        origin_task.await.expect("origin task");
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_head_closes_thirty_seconds_after_its_first_octet() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let context = direct_context(directory.path(), &shutdown);
        let (mut client, server_stream) = tokio::io::duplex(1024);
        let server = tokio::spawn(serve_test_connection(server_stream, context, shutdown));

        client.write_all(b"G").await.expect("partial head");
        tokio::task::yield_now().await;
        tokio::time::advance(REQUEST_HEAD_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        let mut received = Vec::new();
        client
            .read_to_end(&mut received)
            .await
            .expect("closed client");
        assert!(received.is_empty());
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_connection_has_no_request_head_deadline_before_its_first_octet() {
        let directory = tempfile::tempdir().expect("test directory");
        let shutdown = Shutdown::new();
        let context = direct_context(directory.path(), &shutdown);
        let (mut client, server_stream) = tokio::io::duplex(2048);
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(serve_test_connection(
            server_stream,
            context,
            server_shutdown,
        ));

        tokio::time::advance(REQUEST_HEAD_TIMEOUT + Duration::from_secs(1)).await;
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .expect("health request");
        let mut response = Vec::new();
        loop {
            response.push(client.read_u8().await.expect("health response"));
            if response.ends_with(b"ok\n") {
                break;
            }
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));
        shutdown.request();
        server.await.expect("server task").expect("server result");
    }

    async fn read_connect_response(client: &mut tokio::io::DuplexStream) {
        let mut response = Vec::new();
        loop {
            let mut byte = [0];
            client
                .read_exact(&mut byte)
                .await
                .expect("CONNECT response");
            response.push(byte[0]);
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert_eq!(response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    }

    #[tokio::test(start_paused = true)]
    async fn public_connect_survives_past_connection_drain_timeout() {
        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let shutdown = crate::Shutdown::new();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::test_with_stream(upstream),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(1024);
        let server =
            tokio::spawn(
                async move { serve_test_connection(server_stream, context, shutdown).await },
            );
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\noptimistic-prefix")
            .await
            .expect("CONNECT request");
        read_connect_response(&mut client).await;
        let mut prefix = [0; 17];
        peer.read_exact(&mut prefix).await.expect("buffered prefix");
        assert_eq!(&prefix, b"optimistic-prefix");
        tokio::task::yield_now().await;
        tokio::time::advance(GRACEFUL_SHUTDOWN_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        client
            .write_all(b"after-timeout")
            .await
            .expect("client tunnel write");
        let mut client_bytes = [0; 13];
        peer.read_exact(&mut client_bytes)
            .await
            .expect("upstream tunnel read");
        assert_eq!(&client_bytes, b"after-timeout");
        peer.write_all(b"upstream-reply")
            .await
            .expect("upstream tunnel write");
        let mut upstream_bytes = [0; 14];
        client
            .read_exact(&mut upstream_bytes)
            .await
            .expect("client tunnel read");
        assert_eq!(&upstream_bytes, b"upstream-reply");
        client.shutdown().await.expect("client shutdown");
        peer.shutdown().await.expect("peer shutdown");
        server
            .await
            .expect("server task")
            .expect("serve connection");
    }

    #[tokio::test]
    async fn client_disconnect_after_success_closes_upstream_and_observes_relay_result() {
        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .expect("test config");
        let shutdown = Shutdown::new();
        let (upstream, mut origin) = tokio::io::duplex(1024);
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::test_with_stream(upstream),
            None,
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(1024);
        let server = tokio::spawn(serve_test_connection(server_stream, context, shutdown));
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n")
            .await
            .expect("CONNECT request");
        read_connect_response(&mut client).await;

        drop(client);

        let mut end = [0_u8; 1];
        assert_eq!(origin.read(&mut end).await.unwrap(), 0);
        origin
            .write_all(b"reverse bytes after client cancellation")
            .await
            .expect("queue reverse bytes");
        let error = server
            .await
            .expect("server task")
            .expect_err("cancelled client makes reverse relay terminal");
        assert_eq!(error.to_string(), "CONNECT tunnel I/O failure");
        assert!(origin.write_all(b"after terminal result").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn local_connect_survives_past_connection_drain_timeout() {
        let directory = tempfile::tempdir().expect("test directory");
        let allowlist = directory.path().join("allowlist");
        let mode = directory.path().join("mode");
        let local = [
            directory.path().join("local-global"),
            directory.path().join("local-project"),
            directory.path().join("local-run"),
        ];
        std::fs::write(&allowlist, "allowed.example\n").expect("allowlist");
        std::fs::write(&mode, "enforce\n").expect("mode");
        std::fs::write(&local[0], "localhost:1234\n").expect("local allowlist");
        std::fs::write(&local[1], "").expect("local allowlist");
        std::fs::write(&local[2], "").expect("local allowlist");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_LOOPBACK_ALLOWLISTS" => Some(
                local
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            "VHRN_BROKER_ADDR" => Some("127.0.0.1:1".to_owned()),
            "VHRN_BROKER_TOKEN_FILE" => Some(directory.path().join("token").display().to_string()),
            _ => None,
        })
        .expect("test config");
        let shutdown = crate::Shutdown::new();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let context = Arc::new(RequestContext::new(
            config,
            crate::connect::public::PublicConnector::system(
                crate::shutdown::ProcessResources::testing(256, 256),
            ),
            Some(crate::connect::broker::BrokerConnector::test_with_connect_stream(upstream)),
            &shutdown,
        ));
        let (mut client, server_stream) = tokio::io::duplex(1024);
        let server =
            tokio::spawn(
                async move { serve_test_connection(server_stream, context, shutdown).await },
            );
        client
            .write_all(b"CONNECT localhost:1234 HTTP/1.1\r\nHost: localhost:1234\r\n\r\n")
            .await
            .expect("CONNECT request");
        read_connect_response(&mut client).await;
        tokio::task::yield_now().await;
        tokio::time::advance(GRACEFUL_SHUTDOWN_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        client
            .write_all(b"local-client")
            .await
            .expect("client tunnel write");
        let mut client_bytes = [0; 12];
        peer.read_exact(&mut client_bytes)
            .await
            .expect("broker tunnel read");
        assert_eq!(&client_bytes, b"local-client");
        peer.write_all(b"local-upstream")
            .await
            .expect("broker tunnel write");
        let mut upstream_bytes = [0; 14];
        client
            .read_exact(&mut upstream_bytes)
            .await
            .expect("client tunnel read");
        assert_eq!(&upstream_bytes, b"local-upstream");
        client.shutdown().await.expect("client shutdown");
        peer.shutdown().await.expect("peer shutdown");
        server
            .await
            .expect("server task")
            .expect("serve connection");
    }
}
