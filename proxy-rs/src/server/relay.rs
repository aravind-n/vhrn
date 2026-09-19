//! Bounded bidirectional stream ownership.

use crate::Shutdown;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Relays both stream directions until EOF, cancellation, or an I/O error.
///
/// The two copies are owned by this future; it never leaves background work behind.
///
/// # Errors
///
/// Returns the first I/O error reported by either direction.
pub async fn relay<A, B>(first: A, second: B, shutdown: Shutdown) -> anyhow::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    if shutdown.is_requested() {
        return Ok(());
    }
    let (mut first_read, mut first_write) = tokio::io::split(first);
    let (mut second_read, mut second_write) = tokio::io::split(second);

    let first_to_second = async {
        tokio::select! {
            result = tokio::io::copy(&mut first_read, &mut second_write) => {
                result.map_err(|error| anyhow::anyhow!(error).context("copy client to upstream"))?;
                second_write.shutdown().await.map_err(|error| anyhow::anyhow!(error).context("shutdown upstream write half"))?;
                Ok(())
            }
            () = shutdown.cancelled() => Ok(()),
        }
    };
    let second_to_first = async {
        tokio::select! {
            result = tokio::io::copy(&mut second_read, &mut first_write) => {
                result.map_err(|error| anyhow::anyhow!(error).context("copy upstream to client"))?;
                first_write.shutdown().await.map_err(|error| anyhow::anyhow!(error).context("shutdown client write half"))?;
                Ok(())
            }
            () = shutdown.cancelled() => Ok(()),
        }
    };
    tokio::try_join!(first_to_second, second_to_first).map(|_| ())
}

#[cfg(test)]
mod tests {
    use crate::Shutdown;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    use super::relay;

    #[tokio::test]
    async fn cancellation_closes_owned_streams() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let shutdown = Shutdown::new();
        let task = tokio::spawn(relay(client, server, shutdown.clone()));
        shutdown.request();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut byte = [0];
        assert_eq!(client_peer.read(&mut byte).await.unwrap(), 0);
        assert_eq!(server_peer.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn already_cancelled_returns_without_waiting() {
        let (first, _) = tokio::io::duplex(1);
        let (second, _) = tokio::io::duplex(1);
        let shutdown = Shutdown::new();
        shutdown.request();
        timeout(Duration::from_secs(1), relay(first, second, shutdown))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn false_notification_keeps_relay_pending_until_cancellation() {
        let (first, mut first_peer) = tokio::io::duplex(1);
        let (second, mut second_peer) = tokio::io::duplex(1);
        let shutdown = Shutdown::new();
        let mut task = tokio::spawn(relay(first, second, shutdown.clone()));
        assert!(
            timeout(Duration::from_millis(100), &mut task)
                .await
                .is_err()
        );
        shutdown.request();
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_millis(500), first_peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(Duration::from_millis(500), second_peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn relays_and_preserves_half_close() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let shutdown = Shutdown::new();
        let task = tokio::spawn(relay(client, server, shutdown));
        client_peer.write_all(b"request").await.unwrap();
        client_peer.shutdown().await.unwrap();
        let mut received = Vec::new();
        server_peer.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"request");
        server_peer.write_all(b"response").await.unwrap();
        server_peer.shutdown().await.unwrap();
        let mut response = Vec::new();
        client_peer.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn upstream_eof_keeps_client_write_half_open() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let shutdown = Shutdown::new();
        let task = tokio::spawn(relay(client, server, shutdown));

        server_peer.write_all(b"sentinel").await.unwrap();
        server_peer.shutdown().await.unwrap();
        let mut received = Vec::new();
        timeout(
            Duration::from_secs(1),
            client_peer.read_to_end(&mut received),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received, b"sentinel");
        client_peer
            .write_all(b"request after upstream eof")
            .await
            .unwrap();
        client_peer.shutdown().await.unwrap();
        let mut request = Vec::new();
        timeout(
            Duration::from_secs(1),
            server_peer.read_to_end(&mut request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(request, b"request after upstream eof");
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
