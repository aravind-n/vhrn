//! HTTP transport over the authenticated host-side broker capability.

use super::protocol::{BrokerError, BrokerProtocol, BrokerStream, BrokerToken};
#[cfg(test)]
use super::protocol::{short_test_deadlines, test_connect_authority, test_connect_frame};

#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use crate::config::BrokerEndpoint;
use crate::connect::forward::{ForwardError, ForwardErrorKind, Forwarded, exchange, forward_error};
use crate::connect::origin_body::{HttpOrigin, OriginLease, take_origin};
use crate::domain::target::{LocalTarget, LoopbackAuthority};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Shutdown;
use crate::connect::pool::IdlePool;
#[cfg(test)]
use crate::connect::pool::{IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME, NonZeroDuration};
use crate::server::http1::{Http1Connection, RequestHead};
use crate::shutdown::ProcessResources;

/// Private connector for the broker capability.
#[derive(Clone)]
pub(crate) struct BrokerConnector {
    protocol: BrokerProtocol,
    pool: IdlePool<BrokerKey, HttpOrigin<BrokerStream>>,
    #[cfg(test)]
    test_connect_stream: Arc<std::sync::Mutex<Option<BrokerStream>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BrokerKey {
    authority: LoopbackAuthority,
}

#[derive(Debug)]
pub(crate) enum BrokerForwardError {
    Connect(BrokerError),
    Exchange(ForwardError),
    Origin(BrokerError, ForwardError),
}

impl BrokerConnector {
    pub(crate) fn new(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        resources: ProcessResources,
    ) -> Self {
        Self {
            protocol: BrokerProtocol::new(endpoint, token, resources),
            pool: IdlePool::new(),
            #[cfg(test)]
            test_connect_stream: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) async fn ready(&self, cancellation: &Shutdown) -> Result<(), BrokerError> {
        self.protocol.ready(cancellation).await
    }

    pub(crate) async fn connect(
        &self,
        authority: &LoopbackAuthority,
        cancellation: &Shutdown,
    ) -> Result<BrokerStream, BrokerError> {
        #[cfg(test)]
        if let Some(stream) = self
            .test_connect_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            if cancellation.is_requested() {
                return Err(BrokerError::Cancelled);
            }
            return Ok(stream);
        }
        self.protocol.connect(authority, cancellation).await
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
            test_connect_stream: Arc::new(std::sync::Mutex::new(None)),
        }
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

    pub(crate) fn prune_pool(&self) {
        self.pool.prune();
    }

    pub(crate) fn close_pool(&self) {
        self.pool.close();
    }
}

impl BrokerConnector {
    pub(crate) async fn forward<D>(
        &self,
        target: &LocalTarget,
        head: &RequestHead,
        downstream: &mut Http1Connection<D>,
        cancellation: &Shutdown,
    ) -> Result<Forwarded, BrokerForwardError>
    where
        D: AsyncRead + AsyncWrite + Unpin,
    {
        let authority = target.canonical_authority();
        let key = BrokerKey {
            authority: authority.clone(),
        };
        let origin = tokio::select! {
            biased;
            _ = downstream.wait_for_peer_close() => {
                return Err(BrokerForwardError::Exchange(forward_error(
                    ForwardErrorKind::ClientDisconnected,
                    false,
                )));
            }
            result = async {
                match take_origin(&self.pool, &key) {
                    Some(origin) => Ok(origin),
                    None => self
                        .connect(authority, cancellation)
                        .await
                        .map(HttpOrigin::new),
                }
            } => result.map_err(BrokerForwardError::Connect)?,
        };
        let lease = OriginLease::new(origin, key, self.pool.clone());
        let canonical = authority.to_string();
        let host = if target.explicit_port() {
            canonical.as_str()
        } else {
            canonical
                .rsplit_once(':')
                .map_or(canonical.as_str(), |(host, _)| host)
        };
        exchange(
            downstream,
            head,
            target.path_and_query(),
            host,
            lease,
            cancellation,
        )
        .await
        .map_err(|error| {
            if error.kind == crate::connect::forward::ForwardErrorKind::BadGateway {
                BrokerForwardError::Origin(BrokerError::OriginFailure, error)
            } else {
                BrokerForwardError::Exchange(error)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use hyper::{HeaderMap, Method, StatusCode, Version, header};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::domain::target::{Target, classify};
    use crate::server::http1::{BodyFraming, Http1Connection, RequestHead};

    #[tokio::test]
    async fn broker_http_uses_the_shared_streaming_forwarder() {
        assert_eq!(
            test_connect_authority(&test_connect_frame("localhost:80")),
            "localhost:80"
        );
        let (proxy_origin, mut origin) = tokio::io::duplex(4096);
        let connector = BrokerConnector::test_with_connect_stream(proxy_origin);
        let target = match classify(&Method::POST, b"http://localhost:80/local") {
            Target::LocalHttp(target) => target,
            other => panic!("unexpected target: {other:?}"),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "conflict.invalid".parse().unwrap());
        headers.insert(header::CONTENT_LENGTH, "4".parse().unwrap());
        let head = RequestHead {
            method: Method::POST,
            raw_target: b"http://localhost:80/local".to_vec(),
            version: Version::HTTP_11,
            headers,
            framing: BodyFraming::ContentLength(4),
            close: false,
            upgrade: false,
            expect: false,
        };
        let (mut client, downstream) = tokio::io::duplex(4096);
        client.write_all(b"data").await.unwrap();
        let mut downstream = Http1Connection::new(downstream);
        let origin_task = tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            loop {
                let read = origin.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|value| value == b"\r\n\r\n")
                    && request.ends_with(b"data")
                {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /local HTTP/1.1\r\n"));
            assert!(request.contains("host: localhost:80\r\n"));
            assert!(!request.contains("conflict.invalid"));
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });
        let response = connector
            .forward(&target, &head, &mut downstream, &Shutdown::new())
            .await
            .unwrap()
            .response;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.body.collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"ok")
        );
        origin_task.await.unwrap();
    }
}
