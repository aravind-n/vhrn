//! Bounded, opaque CONNECT tunnel ownership.

use std::fmt;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Shutdown;

const RELAY_BUFFER_BYTES: usize = 32 * 1024;
const MAX_DIRECTION_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PREFIX_BYTES: usize = MAX_DIRECTION_BUFFER_BYTES - RELAY_BUFFER_BYTES;

/// Streams and bytes retained while establishing one checked tunnel.
pub(crate) struct TunnelParts<D, U> {
    pub(crate) downstream: D,
    pub(crate) downstream_prefix: Bytes,
    pub(crate) upstream: U,
    pub(crate) upstream_prefix: Bytes,
}

/// A redacted terminal category for a nonrecoverable tunnel failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelayError;

impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CONNECT tunnel I/O failure")
    }
}

impl std::error::Error for RelayError {}

/// Relays both stream directions until EOF, cancellation, or an I/O error.
///
/// The two streams and both eager prefixes are owned by this future; it never leaves background
/// work behind or reports raw I/O details.
pub(crate) async fn relay<D, U>(
    parts: TunnelParts<D, U>,
    shutdown: Shutdown,
) -> Result<(), RelayError>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    if shutdown.is_requested() {
        return Ok(());
    }
    let TunnelParts {
        downstream,
        downstream_prefix,
        upstream,
        upstream_prefix,
    } = parts;
    if downstream_prefix.len() > MAX_PREFIX_BYTES || upstream_prefix.len() > MAX_PREFIX_BYTES {
        return Err(RelayError);
    }
    let (mut downstream_read, mut downstream_write) = tokio::io::split(downstream);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);

    let downstream_to_upstream = async {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => Ok(()),
            result = copy_direction(
                &mut downstream_read,
                &mut upstream_write,
                downstream_prefix,
            ) => result,
        }
    };
    let upstream_to_downstream = async {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => Ok(()),
            result = copy_direction(
                &mut upstream_read,
                &mut downstream_write,
                upstream_prefix,
            ) => result,
        }
    };
    tokio::try_join!(downstream_to_upstream, upstream_to_downstream).map(|_| ())
}

async fn copy_direction<R, W>(
    reader: &mut R,
    writer: &mut W,
    prefix: Bytes,
) -> Result<(), RelayError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer.write_all(&prefix).await.map_err(|_| RelayError)?;
    let mut buffer = vec![0_u8; RELAY_BUFFER_BYTES].into_boxed_slice();
    loop {
        let count = reader.read(&mut buffer).await.map_err(|_| RelayError)?;
        if count == 0 {
            writer.flush().await.map_err(|_| RelayError)?;
            writer.shutdown().await.map_err(|_| RelayError)?;
            return Ok(());
        }
        writer
            .write_all(&buffer[..count])
            .await
            .map_err(|_| RelayError)?;
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    use super::{RelayError, TunnelParts, relay};
    use crate::Shutdown;

    fn parts<D, U>(downstream: D, upstream: U) -> TunnelParts<D, U> {
        TunnelParts {
            downstream,
            downstream_prefix: Bytes::new(),
            upstream,
            upstream_prefix: Bytes::new(),
        }
    }

    #[tokio::test]
    async fn cancellation_closes_owned_streams() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let shutdown = Shutdown::new();
        let task = tokio::spawn(relay(parts(client, server), shutdown.clone()));
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
        timeout(
            Duration::from_secs(1),
            relay(parts(first, second), shutdown),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_tunnel_has_no_timeout() {
        let (first, mut first_peer) = tokio::io::duplex(1);
        let (second, mut second_peer) = tokio::io::duplex(1);
        let shutdown = Shutdown::new();
        let mut task = tokio::spawn(relay(parts(first, second), shutdown.clone()));

        tokio::time::advance(Duration::from_hours(365 * 24)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        shutdown.request();
        let mut byte = [0];
        assert_eq!(first_peer.read(&mut byte).await.unwrap(), 0);
        timeout(Duration::from_secs(1), &mut task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second_peer.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn eager_prefixes_are_first_and_lossless_in_both_directions() {
        let (downstream, mut client) = tokio::io::duplex(256);
        let (upstream, mut origin) = tokio::io::duplex(256);
        let downstream_prefix = Bytes::from(vec![0x16; 12 * 1024]);
        let upstream_prefix = Bytes::from(vec![0x17; 12 * 1024]);
        let expected_downstream = downstream_prefix.clone();
        let expected_upstream = upstream_prefix.clone();
        let task = tokio::spawn(relay(
            TunnelParts {
                downstream,
                downstream_prefix,
                upstream,
                upstream_prefix,
            },
            Shutdown::new(),
        ));

        let client_task = tokio::spawn(async move {
            client.write_all(b"client-suffix").await.unwrap();
            client.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let origin_task = tokio::spawn(async move {
            origin.write_all(b"origin-suffix").await.unwrap();
            origin.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            origin.read_to_end(&mut bytes).await.unwrap();
            bytes
        });

        let mut from_upstream = expected_upstream.to_vec();
        from_upstream.extend_from_slice(b"origin-suffix");
        let mut from_downstream = expected_downstream.to_vec();
        from_downstream.extend_from_slice(b"client-suffix");
        assert_eq!(client_task.await.unwrap(), from_upstream);
        assert_eq!(origin_task.await.unwrap(), from_downstream);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn simultaneous_traffic_and_queued_bytes_survive_client_eof() {
        let (downstream, client) = tokio::io::duplex(1024);
        let (upstream, origin) = tokio::io::duplex(1024);
        let task = tokio::spawn(relay(parts(downstream, upstream), Shutdown::new()));
        let client_bytes = vec![0x31; 128 * 1024];
        let origin_bytes = vec![0x32; 128 * 1024];
        let expected_client = client_bytes.clone();
        let expected_origin = origin_bytes.clone();
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut origin_read, mut origin_write) = tokio::io::split(origin);

        let client_writer = tokio::spawn(async move {
            client_write.write_all(&client_bytes).await.unwrap();
            client_write.shutdown().await.unwrap();
        });
        let origin_writer = tokio::spawn(async move {
            origin_write.write_all(&origin_bytes).await.unwrap();
            origin_write.shutdown().await.unwrap();
        });
        let client_reader = tokio::spawn(async move {
            let mut response = Vec::new();
            client_read.read_to_end(&mut response).await.unwrap();
            response
        });
        let origin_reader = tokio::spawn(async move {
            let mut request = Vec::new();
            origin_read.read_to_end(&mut request).await.unwrap();
            request
        });

        client_writer.await.unwrap();
        origin_writer.await.unwrap();
        assert_eq!(client_reader.await.unwrap(), expected_origin);
        assert_eq!(origin_reader.await.unwrap(), expected_client);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn upstream_eof_keeps_client_write_half_open() {
        let (downstream, mut client) = tokio::io::duplex(64);
        let (upstream, mut origin) = tokio::io::duplex(64);
        let task = tokio::spawn(relay(parts(downstream, upstream), Shutdown::new()));

        origin.write_all(b"sentinel").await.unwrap();
        origin.shutdown().await.unwrap();
        let mut received = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, b"sentinel");
        client
            .write_all(b"request after upstream eof")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut request = Vec::new();
        timeout(Duration::from_secs(1), origin.read_to_end(&mut request))
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

    #[tokio::test]
    async fn nonrecoverable_io_error_is_redacted_and_closes_both_streams() {
        let (downstream, client) = tokio::io::duplex(8);
        let (upstream, mut origin) = tokio::io::duplex(8);
        drop(client);
        let task = tokio::spawn(relay(parts(downstream, upstream), Shutdown::new()));

        let _ = origin.write_all(b"trigger write failure").await;
        let error = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .expect_err("relay failure");
        assert_eq!(error, RelayError);
        assert_eq!(error.to_string(), "CONNECT tunnel I/O failure");
        let mut byte = [0];
        assert_eq!(origin.read(&mut byte).await.unwrap(), 0);
    }
}
