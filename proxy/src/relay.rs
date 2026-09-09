//! Bounded bidirectional stream ownership.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

/// Relays both stream directions until EOF, cancellation, or an I/O error.
///
/// The two copies are owned by this future; it never leaves background work behind.
///
/// # Errors
///
/// Returns the first I/O error reported by either direction.
pub async fn relay<A, B>(
    mut first: A,
    mut second: B,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    if *shutdown.borrow() {
        return Ok(());
    }
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut first, &mut second) => result.map(|_| ()),
        () = cancelled(&mut shutdown) => {
            Ok(())
        }
    }
}

async fn cancelled(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::watch;
    use tokio::time::{Duration, timeout};

    use super::relay;

    #[tokio::test]
    async fn cancellation_closes_owned_streams() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let (shutdown, _) = watch::channel(false);
        let task = tokio::spawn(relay(client, server, shutdown.subscribe()));
        shutdown.send(true).unwrap();
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
        let (shutdown, _) = watch::channel(true);
        timeout(
            Duration::from_secs(1),
            relay(first, second, shutdown.subscribe()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn false_notification_keeps_relay_pending_until_cancellation() {
        let (first, mut first_peer) = tokio::io::duplex(1);
        let (second, mut second_peer) = tokio::io::duplex(1);
        let (shutdown, receiver) = watch::channel(false);
        let mut task = tokio::spawn(relay(first, second, receiver));
        shutdown.send(false).unwrap();
        assert!(
            timeout(Duration::from_millis(100), &mut task)
                .await
                .is_err()
        );
        shutdown.send(true).unwrap();
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
        let (shutdown, _) = watch::channel(false);
        let task = tokio::spawn(relay(client, server, shutdown.subscribe()));
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
}
