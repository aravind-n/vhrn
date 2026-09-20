//! Listener startup and HTTP connection serving.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::{
    Bootstrap, Shutdown,
    server::router::{RequestContext, handle},
};

const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_CONNECTIONS: usize = 64;
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
        shutdown,
    } = bootstrap;
    let context = Arc::new(RequestContext::with_services(
        config,
        public,
        local,
        shutdown.clone(),
        audit,
        health,
    ));
    let admission = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = tokio::task::JoinSet::new();
    let outcome = loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break Ok(()),
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(joined) = joined {
                    report_task_result(joined, false, "join proxy connection");
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Some(permit) = acquire_connection_permit(&admission) else {
                        continue;
                    };
                    let context = context.clone();
                    let shutdown = shutdown.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        serve_connection_io(stream, context, shutdown).await
                    });
                }
                Err(_) => break Err(anyhow!("listener_accept_failed")),
            },
        }
    };
    drain_connections(&mut connections, CONNECTION_DRAIN_TIMEOUT).await;
    outcome
}

fn acquire_connection_permit(
    admission: &Arc<Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    admission.clone().try_acquire_owned().ok()
}
#[cfg(test)]
pub(crate) async fn serve_test_connection(
    stream: tokio::io::DuplexStream,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<()> {
    serve_connection_io(stream, context, shutdown).await
}

async fn serve_connection_io<S>(
    stream: S,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    if shutdown.is_requested() {
        return Ok(());
    }
    let tunnels = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
    let service_tunnels = tunnels.clone();
    let service = service_fn(move |request| {
        let context = context.clone();
        let tunnels = service_tunnels.clone();
        async move { Ok::<_, hyper::Error>(handle(request, context, tunnels).await) }
    });
    let mut connection = Box::pin(
        http1::Builder::new()
            .max_headers(64)
            .max_buf_size(16 * 1024)
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades(),
    );
    let (result, terminating) = tokio::select! {
        result = &mut connection => {
            let result = result.context("serve proxy connection");
            let terminating = result.is_err();
            (result, terminating)
        }
        () = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            match tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, &mut connection).await {
                Ok(Ok(())) | Err(_) => {}
                Ok(Err(error)) => crate::diagnostics::report(
                    &anyhow::Error::new(error).context("drain proxy connection during shutdown"),
                ),
            }
            (Ok(()), true)
        }
    };
    let mut tunnels = tunnels.lock().await;
    if terminating {
        drain_tunnels(&mut tunnels, CONNECTION_DRAIN_TIMEOUT).await;
    } else {
        observe_tunnels_until_shutdown(&mut tunnels, &shutdown).await;
        if shutdown.is_requested() {
            drain_tunnels(&mut tunnels, CONNECTION_DRAIN_TIMEOUT).await;
        }
    }
    result
}

/// Keeps completed CONNECT relays observed while their HTTP connection has ended.
///
/// An HTTP/1 CONNECT response completes before the upgraded stream does.  A normal
/// connection completion is therefore not a lifecycle boundary for its tunnels.
async fn observe_tunnels_until_shutdown(
    tunnels: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    shutdown: &Shutdown,
) {
    while !tunnels.is_empty() {
        tokio::select! {
            () = shutdown.cancelled() => break,
            joined = tunnels.join_next() => {
                if let Some(joined) = joined {
                    report_task_result(joined, false, "join CONNECT tunnel");
                }
            }
        }
    }
}

/// Wait briefly for CONNECT relays, then cancel and observe every remaining task.
async fn drain_tunnels(
    tunnels: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    deadline: Duration,
) -> DrainSummary {
    drain_tasks(tunnels, deadline, "join CONNECT tunnel").await
}

async fn drain_connections(
    connections: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    deadline: Duration,
) -> DrainSummary {
    drain_tasks(connections, deadline, "join proxy connection").await
}

async fn drain_tasks(
    tasks: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    deadline: Duration,
    join_context: &'static str,
) -> DrainSummary {
    let mut summary = DrainSummary::default();
    let until = tokio::time::Instant::now() + deadline;
    while !tasks.is_empty() {
        let joined = match tokio::time::timeout_at(until, tasks.join_next()).await {
            Ok(Some(joined)) => joined,
            Ok(None) => return summary,
            Err(_) => break,
        };
        summary.record(report_task_result(joined, false, join_context));
    }
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
    fn record(&mut self, outcome: TunnelOutcome) {
        match outcome {
            TunnelOutcome::Completed => self.completed += 1,
            TunnelOutcome::Failed => self.failed += 1,
            TunnelOutcome::JoinFailed => self.join_failed += 1,
            TunnelOutcome::Aborted => self.aborted += 1,
        }
    }
}

#[derive(Clone, Copy)]
enum TunnelOutcome {
    Completed,
    Failed,
    JoinFailed,
    Aborted,
}

fn report_task_result(
    joined: Result<anyhow::Result<()>, tokio::task::JoinError>,
    aborted: bool,
    join_context: &'static str,
) -> TunnelOutcome {
    match joined {
        Ok(Ok(())) => TunnelOutcome::Completed,
        Ok(Err(error)) => {
            crate::diagnostics::report(&error);
            TunnelOutcome::Failed
        }
        Err(error) if aborted && error.is_cancelled() => TunnelOutcome::Aborted,
        Err(error) => {
            crate::diagnostics::report(&anyhow::Error::new(error).context(join_context));
            TunnelOutcome::JoinFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::pending,
        sync::atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct PendingTask(Arc<AtomicBool>);

    impl Drop for PendingTask {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
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

        let summary = drain_tunnels(&mut tunnels, Duration::from_secs(1)).await;

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

        let summary = drain_connections(&mut connections, Duration::from_secs(1)).await;

        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.join_failed, 1);
        assert_eq!(summary.aborted, 1);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(connections.is_empty());
    }

    #[test]
    fn admission_limit_rejects_the_sixty_fifth_connection() {
        let admission = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let permits: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| acquire_connection_permit(&admission))
            .collect();

        assert!(permits.iter().all(Option::is_some));
        assert!(acquire_connection_permit(&admission).is_none());
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
        assert!(
            std::str::from_utf8(&response)
                .expect("response text")
                .starts_with("HTTP/1.1 200")
        );
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
            shutdown.clone(),
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
        tokio::time::advance(CONNECTION_DRAIN_TIMEOUT + Duration::from_millis(1)).await;
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
            crate::connect::public::PublicConnector::system(),
            Some(crate::connect::broker::BrokerConnector::test_with_connect_stream(upstream)),
            shutdown.clone(),
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
        tokio::time::advance(CONNECTION_DRAIN_TIMEOUT + Duration::from_millis(1)).await;
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
