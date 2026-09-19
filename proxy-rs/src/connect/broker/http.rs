//! HTTP transport over the authenticated host-side broker capability.

use super::protocol::{BrokerProtocol, BrokerStream, BrokerToken};
#[cfg(test)]
use super::protocol::{short_test_deadlines, test_connect_authority, test_connect_frame};

use std::sync::Arc;
use std::time::Duration;

use crate::config::BrokerEndpoint;
use crate::connect::origin_body::{
    BoundedOriginBody, OriginBodyLimits, OriginResponse, SharedOriginResponse,
};
use crate::domain::target::LoopbackAuthority;
use crate::headers::sanitize_hop_by_hop;
use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
#[cfg(test)]
use hyper::StatusCode;
use hyper::client::conn::http1;
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::connect::pool::IdlePool;
#[cfg(test)]
use crate::connect::pool::{IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME, NonZeroDuration};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Private connector for the broker capability.
#[derive(Clone)]
pub(crate) struct BrokerConnector {
    protocol: BrokerProtocol,
    pool: IdlePool<BrokerKey, BrokerConnection>,
    http_timeout: Duration,
    response_limit: usize,
    tls_config: Arc<rustls::ClientConfig>,
    #[cfg(test)]
    active_drivers: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    test_connect_stream: Arc<std::sync::Mutex<Option<BrokerStream>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BrokerKey {
    secure: bool,
    authority: LoopbackAuthority,
}

struct BrokerConnection {
    sender: http1::SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

fn broker_connection_reusable(connection: &BrokerConnection) -> bool {
    !connection.driver.is_finished() && connection.sender.is_ready()
}

#[cfg(test)]
struct DriverGuard(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(test)]
impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for BrokerConnection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl BrokerConnector {
    pub(crate) fn with_tls_config(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Self {
        Self {
            protocol: BrokerProtocol::new(endpoint, token),
            pool: IdlePool::new(),
            http_timeout: HTTP_TIMEOUT,
            response_limit: MAX_HTTP_RESPONSE_BYTES,
            tls_config,
            #[cfg(test)]
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_connect_stream: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) async fn ready(&self) -> Result<()> {
        self.protocol.ready().await
    }

    pub(crate) async fn connect(&self, authority: &LoopbackAuthority) -> Result<BrokerStream> {
        #[cfg(test)]
        if let Some(stream) = self
            .test_connect_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Ok(stream);
        }
        self.protocol.connect(authority).await
    }
    #[cfg(test)]
    fn with_deadlines_and_pool(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        capacity: usize,
        lifetime: Duration,
    ) -> Self {
        Self {
            protocol: BrokerProtocol::with_deadlines(endpoint, token, short_test_deadlines()),
            pool: IdlePool::with_limits(
                std::num::NonZeroUsize::new(capacity).expect("test capacity is nonzero"),
                NonZeroDuration::new(lifetime).expect("test lifetime is nonzero"),
            ),
            http_timeout: HTTP_TIMEOUT,
            response_limit: MAX_HTTP_RESPONSE_BYTES,
            tls_config: crate::connect::tls::production_client_config()
                .expect("test TLS configuration"),
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            test_connect_stream: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    fn with_http_limits(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        http_timeout: Duration,
        response_limit: usize,
    ) -> Self {
        let mut connector =
            Self::with_deadlines_and_pool(endpoint, token, IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME);
        connector.http_timeout = http_timeout;
        connector.response_limit = response_limit;
        connector
    }
    #[cfg(test)]
    pub(crate) fn pool_len(&self) -> usize {
        self.pool.len()
    }
    #[cfg(test)]
    pub(crate) fn active_drivers(&self) -> usize {
        self.active_drivers
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(test)]
    fn clear_pool(&self) {
        self.pool.clear();
    }

    #[cfg(test)]
    pub(crate) fn test_with_connect_stream(stream: tokio::io::DuplexStream) -> Self {
        let connector = Self::with_deadlines_and_pool(
            "127.0.0.1:1"
                .parse::<std::net::SocketAddr>()
                .expect("test address"),
            "a".repeat(64).parse().expect("test token"),
            IDLE_POOL_CAPACITY,
            IDLE_POOL_LIFETIME,
        );
        *connector
            .test_connect_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(BrokerStream::test_with_stream(stream));
        connector
    }
}

pub(crate) type BrokerResponse = SharedOriginResponse;

impl BrokerConnector {
    pub(crate) async fn http(
        &self,
        authority: &LoopbackAuthority,
        secure: bool,
        request: Request<Full<Bytes>>,
    ) -> Result<BrokerResponse> {
        let key = BrokerKey {
            secure,
            authority: authority.clone(),
        };
        let connection = self.pool.take_if_reusable(&key, broker_connection_reusable);
        let mut connection = match connection {
            Some(connection) => connection,
            None => self.open_http(authority, secure).await?,
        };
        let (mut parts, body) = request.into_parts();
        sanitize_hop_by_hop(&mut parts.headers);
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        parts.uri = path.parse::<Uri>()?;
        let response = timeout(
            self.http_timeout,
            connection
                .sender
                .send_request(Request::from_parts(parts, body)),
        )
        .await
        .with_context(|| format!("local origin response timed out for {authority}"))?
        .with_context(|| format!("receive local origin response from {authority}"))?;
        let (parts, incoming) = response.into_parts();
        Ok(OriginResponse {
            status: parts.status,
            headers: parts.headers,
            body: BoundedOriginBody::new(
                incoming,
                connection,
                key,
                self.pool.clone(),
                broker_connection_reusable,
                OriginBodyLimits {
                    origin: authority.to_string(),
                    timeout: self.http_timeout,
                    limit: self.response_limit,
                },
            )
            .boxed_unsync(),
        })
    }

    async fn open_http(
        &self,
        authority: &LoopbackAuthority,
        secure: bool,
    ) -> Result<BrokerConnection> {
        let stream = self.connect(authority).await?;
        let stream: Box<dyn AsyncReadWrite> = if secure {
            let name = crate::connect::tls::local_server_name(authority)?;
            Box::new(
                timeout(
                    HTTP_TIMEOUT,
                    TlsConnector::from(self.tls_config.clone()).connect(name, stream),
                )
                .await
                .map_err(|_| anyhow::anyhow!("local TLS handshake timeout"))
                .context("TLS handshake")??,
            )
        } else {
            Box::new(stream)
        };
        let (sender, connection) =
            timeout(self.http_timeout, http1::handshake(TokioIo::new(stream)))
                .await
                .map_err(|_| anyhow::anyhow!("local origin handshake timeout"))
                .context("Hyper handshake")??;
        #[cfg(test)]
        let guard = {
            self.active_drivers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            DriverGuard(self.active_drivers.clone())
        };
        let driver = tokio::spawn(async move {
            #[cfg(test)]
            let _guard = guard;
            let _ = connection.await;
        });
        Ok(BrokerConnection { sender, driver })
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;
    use tokio::net::TcpStream;

    fn token() -> BrokerToken {
        "a".repeat(64).parse().unwrap()
    }
    async fn listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0; 256];
        loop {
            let read = timeout(Duration::from_millis(500), stream.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(read, 0, "origin closed before completing request");
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= 16 * 1024, "request exceeds test bound");
            let Some(headers_end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..headers_end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= headers_end + 4 + length {
                return bytes;
            }
        }
    }

    async fn wait_for_no_drivers(connector: &BrokerConnector) {
        timeout(Duration::from_millis(500), async {
            while connector.active_drivers() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn local_request() -> Request<Full<Bytes>> {
        Request::builder()
            .uri("http://localhost:80/")
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    fn authority_request(authority: &LoopbackAuthority, path: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .uri(format!("http://{authority}{path}"))
            .header("host", authority.to_string())
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn accept_broker(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut frame = vec![0; 100];
        stream.read_exact(&mut frame).await.unwrap();
        stream.write_all(b"OK\n").await.unwrap();
        let _ = read_http_request(&mut stream).await;
        stream
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn bounded_idle_pool_retires_broker_drivers_and_reaps_without_checkout() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let server_release = release.clone();
        let server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let release = server_release.clone();
                handlers.push(tokio::spawn(async move {
                    let mut frame = vec![0; 100];
                    timeout(Duration::from_millis(200), stream.read_exact(&mut frame))
                        .await
                        .unwrap()
                        .unwrap();
                    let frame = String::from_utf8(frame).unwrap();
                    let authority = test_connect_authority(&frame);
                    assert!((80..85).any(|port| authority == format!("localhost:{port}")));
                    let port = authority.strip_prefix("localhost:").unwrap();
                    stream.write_all(b"OK\n").await.unwrap();
                    for attempt in 0..if port == "81" { 2 } else { 1 } {
                        let request =
                            String::from_utf8(read_http_request(&mut stream).await).unwrap();
                        assert!(
                            request
                                .starts_with(&format!("GET /churn/{port}/{attempt} HTTP/1.1\r\n"))
                        );
                        assert!(request.contains(&format!("host: {authority}\r\n")));
                        stream
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    }
                    release.notified().await;
                }));
            }
            server_release.notified().await;
            for handler in handlers {
                handler.await.unwrap();
            }
        });
        let connector =
            BrokerConnector::with_deadlines_and_pool(address, token(), 2, Duration::from_secs(5));
        let authorities = (80..85)
            .map(|port| LoopbackAuthority::parse(&format!("localhost:{port}")).unwrap())
            .collect::<Vec<_>>();
        for authority in authorities.iter().take(3) {
            let port = authority.to_string().rsplit(':').next().unwrap().to_owned();
            let response = connector
                .http(
                    authority,
                    false,
                    authority_request(authority, &format!("/churn/{port}/0")),
                )
                .await
                .unwrap();
            response.body.collect().await.unwrap();
        }
        assert!(connector.pool_len() <= 2);
        let response = connector
            .http(
                &authorities[1],
                false,
                authority_request(&authorities[1], "/churn/81/1"),
            )
            .await
            .unwrap();
        response.body.collect().await.unwrap();
        assert_eq!(connector.pool_len(), 2);
        let response = connector
            .http(
                &authorities[3],
                false,
                authority_request(&authorities[3], "/churn/83/0"),
            )
            .await
            .unwrap();
        response.body.collect().await.unwrap();
        assert!(connector.pool_len() <= 2);
        let response = connector
            .http(
                &authorities[4],
                false,
                authority_request(&authorities[4], "/churn/84/0"),
            )
            .await
            .unwrap();
        response.body.collect().await.unwrap();
        assert_eq!(connector.pool_len(), 2);
        release.notify_waiters();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn broker_idle_pool_expires_without_a_later_request() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = vec![0; 100];
            stream.read_exact(&mut frame).await.unwrap();
            assert_eq!(
                String::from_utf8(frame).unwrap(),
                test_connect_frame("localhost:80")
            );
            stream.write_all(b"OK\n").await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            timeout(Duration::from_millis(500), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap()
        });
        let connector = BrokerConnector::with_deadlines_and_pool(
            address,
            token(),
            2,
            Duration::from_millis(20),
        );
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        response.body.collect().await.unwrap();
        wait_for_no_drivers(&connector).await;
        assert_eq!(connector.pool_len(), 0);
        assert_eq!(server.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn local_http_streams_reuses_and_forwards_exactly() {
        let listener = Arc::new(listener().await);
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server_listener = listener.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = server_listener.accept().await.unwrap();
            let mut frame = vec![0; 100];
            stream.read_exact(&mut frame).await.unwrap();
            assert_eq!(
                String::from_utf8(frame).unwrap(),
                test_connect_frame("localhost:80")
            );
            stream.write_all(b"OK\n").await.unwrap();
            let request = read_http_request(&mut stream).await;
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("POST /path?q=one HTTP/1.1\r\n"));
            assert!(request.contains("host: localhost:80"));
            assert!(!request.to_ascii_lowercase().contains("connection:"));
            assert!(!request.contains("x-remove:"));
            assert!(request.contains("x-ordinary: kept"));
            assert!(!request.contains("proxy-connection"));
            assert!(!request.contains("proxy-authorization"));
            assert!(request.ends_with("exact body"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n")
                .await
                .unwrap();
            released.await.unwrap();
            stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n").await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with(b"GET /again HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(200), 1024);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("http://localhost:80/path?q=one")
            .header("host", "localhost:80")
            .header("connection", "X-Remove")
            .header("x-remove", "removed")
            .header("x-ordinary", "kept")
            .header("proxy-connection", "close")
            .header("proxy-authorization", "ignored")
            .body(Full::new(Bytes::from_static(b"exact body")))
            .unwrap();
        let mut response = connector.http(&authority, false, request).await.unwrap();
        let first = timeout(Duration::from_millis(100), response.body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first, "first");
        release.send(()).unwrap();
        assert_eq!(response.body.collect().await.unwrap().to_bytes(), "second");
        assert_eq!(connector.pool_len(), 1);
        assert_eq!(connector.active_drivers(), 1);
        let second = Request::builder()
            .uri("http://localhost:80/again")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let second = connector.http(&authority, false, second).await.unwrap();
        assert_eq!(second.status, StatusCode::NO_CONTENT);
        assert!(second.body.collect().await.unwrap().to_bytes().is_empty());
        assert_eq!(connector.pool_len(), 1);
        assert_eq!(connector.active_drivers(), 1);
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        connector.clear_pool();
        server.await.unwrap();
        for _ in 0..8 {
            if connector.active_drivers() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(connector.active_drivers(), 0);
    }

    #[tokio::test]
    async fn local_body_drop_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        drop(response);
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_truncation_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(
            response
                .body
                .frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .is_ok()
        );
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_timeout_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(20), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_limit_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 2);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }
}
