//! Shared bounded streaming ownership for HTTP origin responses.

use std::hash::Hash;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Frame, Incoming};
use tokio::time::Sleep;

use crate::connect::pool::IdlePool;

/// An origin response whose body retains the checked-out connection.
pub(crate) struct OriginResponse<B> {
    pub(crate) status: hyper::StatusCode,
    pub(crate) headers: hyper::HeaderMap,
    pub(crate) body: B,
}

pub(crate) type OriginBody = UnsyncBoxBody<Bytes, anyhow::Error>;
pub(crate) type SharedOriginResponse = OriginResponse<OriginBody>;

pub(crate) struct OriginBodyLimits {
    pub(crate) origin: String,
    pub(crate) timeout: Duration,
    pub(crate) limit: usize,
}

/// Streams an origin body while keeping its connection out of the idle pool.
///
/// Dropping this value drops the connection. A connection is returned only after
/// Hyper reports a clean end-of-stream and the connector still considers it reusable.
pub(crate) struct BoundedOriginBody<K, C> {
    incoming: Incoming,
    connection: Option<C>,
    key: K,
    pool: IdlePool<K, C>,
    reusable: fn(&C) -> bool,
    origin: String,
    bytes: usize,
    limit: usize,
    timeout: Duration,
    timer: Pin<Box<Sleep>>,
    ended: bool,
}

impl<K, C> Unpin for BoundedOriginBody<K, C> {}

impl<K, C> BoundedOriginBody<K, C>
where
    K: Clone + Eq + Hash + Send + 'static,
    C: Send + 'static,
{
    pub(crate) fn new(
        incoming: Incoming,
        connection: C,
        key: K,
        pool: IdlePool<K, C>,
        reusable: fn(&C) -> bool,
        limits: OriginBodyLimits,
    ) -> Self {
        Self {
            incoming,
            connection: Some(connection),
            key,
            pool,
            reusable,
            origin: limits.origin,
            bytes: 0,
            limit: limits.limit,
            timeout: limits.timeout,
            timer: Box::pin(tokio::time::sleep(limits.timeout)),
            ended: false,
        }
    }

    fn discard(&mut self) {
        self.connection.take();
    }

    fn return_connection(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool
                .put_if_reusable(self.key.clone(), connection, self.reusable);
        }
    }
}

impl<K, C> Body for BoundedOriginBody<K, C>
where
    K: Clone + Eq + Hash + Send + 'static,
    C: Send + 'static,
{
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.incoming).poll_frame(cx) {
            Poll::Pending if this.timer.as_mut().poll(cx).is_ready() => {
                this.discard();
                this.ended = true;
                Poll::Ready(Some(Err(anyhow::anyhow!(
                    "origin body timed out for {}",
                    this.origin
                ))))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let remaining = this.limit.saturating_sub(this.bytes);
                    if data.len() > remaining {
                        this.discard();
                        this.ended = true;
                        return Poll::Ready(Some(Err(anyhow::anyhow!(
                            "origin body too large for {}",
                            this.origin
                        ))));
                    }
                    this.bytes += data.len();
                }
                this.timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.timeout);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.discard();
                this.ended = true;
                Poll::Ready(Some(Err(anyhow::Error::new(error).context(format!(
                    "receive origin body frame from {}",
                    this.origin
                )))))
            }
            Poll::Ready(None) => {
                this.return_connection();
                this.ended = true;
                Poll::Ready(None)
            }
        }
    }
}
