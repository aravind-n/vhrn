//! Listener startup and HTTP connection serving.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::{
    Shutdown,
    diagnostics::report,
    server::router::{RequestContext, handle},
};

const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_CONNECTIONS: usize = 64;

pub(crate) async fn bind(address: std::net::SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(address)
        .await
        .with_context(|| format!("bind proxy listener at {address}"))
}

pub(crate) async fn serve(
    listener: TcpListener,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<()> {
    let admission = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = tokio::task::JoinSet::new();
    let outcome = loop {
        tokio::select! {
            () = shutdown.cancelled() => break Ok(()),
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(joined) = joined {
                    report_connection(joined);
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = admission.clone().try_acquire_owned() else {
                        continue;
                    };
                    let context = context.clone();
                    let shutdown = shutdown.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        serve_connection(stream, context, shutdown).await
                    });
                }
                Err(error) => break Err(anyhow::Error::new(error).context("accept proxy connection")),
            },
        }
    };
    drain_connections(&mut connections).await;
    outcome
}

async fn serve_connection(
    stream: TcpStream,
    context: Arc<RequestContext>,
    shutdown: Shutdown,
) -> Result<()> {
    let service = service_fn(move |request| {
        let context = context.clone();
        async move { Ok::<_, hyper::Error>(handle(request, context).await) }
    });
    let mut connection = Box::pin(
        http1::Builder::new()
            .max_headers(64)
            .max_buf_size(16 * 1024)
            .serve_connection(TokioIo::new(stream), service),
    );
    tokio::select! {
        result = &mut connection => result.context("serve proxy connection"),
        () = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            match tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, &mut connection).await {
                Ok(result) => result.context("drain proxy connection during shutdown"),
                Err(_) => Ok(()),
            }
        }
    }
}

async fn drain_connections(connections: &mut tokio::task::JoinSet<Result<()>>) {
    let until = tokio::time::Instant::now() + CONNECTION_DRAIN_TIMEOUT;
    while !connections.is_empty() {
        match tokio::time::timeout_at(until, connections.join_next()).await {
            Ok(Some(joined)) => report_connection(joined),
            Ok(None) => return,
            Err(_) => break,
        }
    }
    connections.abort_all();
    while let Some(joined) = connections.join_next().await {
        if !matches!(&joined, Err(error) if error.is_cancelled()) {
            report_connection(joined);
        }
    }
}

fn report_connection(joined: Result<Result<()>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report(&error),
        Err(error) => report(&anyhow::Error::new(error).context("join proxy connection")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_limit_rejects_the_sixty_fifth_connection() {
        let admission = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let permits: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| admission.clone().try_acquire_owned())
            .collect();
        assert!(permits.iter().all(Result::is_ok));
        assert!(admission.clone().try_acquire_owned().is_err());
    }
}
