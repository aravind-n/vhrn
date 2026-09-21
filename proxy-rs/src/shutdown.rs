//! Process-wide cancellation and resource ownership.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

pub(crate) const MAX_CLIENT_CONNECTIONS: usize = 256;
pub(crate) const MAX_UPSTREAM_CONNECTIONS: usize = 256;

#[derive(Clone)]
pub struct Shutdown {
    drain: watch::Sender<bool>,
    force: watch::Sender<bool>,
    requested_at: Arc<Mutex<Option<tokio::time::Instant>>>,
}

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        let (drain, _) = watch::channel(false);
        let (force, _) = watch::channel(false);
        Self {
            drain,
            force,
            requested_at: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.drain.borrow()
    }

    #[must_use]
    pub(crate) fn is_forced(&self) -> bool {
        *self.force.borrow()
    }

    pub fn request(&self) {
        let mut requested_at = self
            .requested_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        requested_at.get_or_insert_with(tokio::time::Instant::now);
        drop(requested_at);
        self.drain.send_replace(true);
    }

    pub(crate) fn deadline(&self, grace: std::time::Duration) -> tokio::time::Instant {
        self.requested_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map_or_else(
                || tokio::time::Instant::now() + grace,
                |started| started + grace,
            )
    }

    #[doc(hidden)]
    pub fn force(&self) {
        self.request();
        self.force.send_replace(true);
    }

    pub async fn cancelled(&self) {
        wait_for(&self.drain).await;
    }

    pub(crate) async fn forced(&self) {
        wait_for(&self.force).await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

async fn wait_for(sender: &watch::Sender<bool>) {
    let mut receiver = sender.subscribe();
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProcessResources {
    client: Admission,
    upstream: Admission,
    sockets: SocketRegistry,
}

impl ProcessResources {
    pub(crate) fn production() -> Self {
        Self::with_limits(MAX_CLIENT_CONNECTIONS, MAX_UPSTREAM_CONNECTIONS)
    }

    fn with_limits(client: usize, upstream: usize) -> Self {
        Self {
            client: Admission::new(client),
            upstream: Admission::new(upstream),
            sockets: SocketRegistry::default(),
        }
    }

    pub(crate) fn try_admit_client(&self) -> Option<SocketLease> {
        let permit = self.client.try_acquire()?;
        Some(SocketLease {
            _permit: permit,
            _registration: self.sockets.register(),
        })
    }

    pub(crate) fn try_upstream(&self) -> Option<CapacityPermit> {
        self.upstream.try_acquire()
    }

    pub(crate) fn manage_upstream<S>(&self, stream: S, permit: CapacityPermit) -> ManagedIo<S> {
        ManagedIo::new(stream, permit, self.sockets.register())
    }

    pub(crate) fn registered_sockets(&self) -> usize {
        self.sockets.active()
    }

    pub(crate) fn force_close_all(&self) {
        self.sockets.force_close_all();
    }

    #[cfg(test)]
    pub(crate) fn testing(client: usize, upstream: usize) -> Self {
        Self::with_limits(client, upstream)
    }

    #[cfg(test)]
    pub(crate) fn client_counts(&self) -> (usize, usize) {
        self.client.counts()
    }

    #[cfg(test)]
    pub(crate) fn upstream_counts(&self) -> (usize, usize) {
        self.upstream.counts()
    }
}

#[derive(Clone)]
struct Admission {
    inner: Arc<AdmissionInner>,
}

struct AdmissionInner {
    semaphore: Arc<Semaphore>,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl Admission {
    fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(AdmissionInner {
                semaphore: Arc::new(Semaphore::new(limit)),
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            }),
        }
    }

    fn try_acquire(&self) -> Option<CapacityPermit> {
        let permit = self.inner.semaphore.clone().try_acquire_owned().ok()?;
        let active = self.inner.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner.maximum.fetch_max(active, Ordering::AcqRel);
        Some(CapacityPermit {
            _permit: permit,
            owner: self.inner.clone(),
        })
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        (
            self.inner.active.load(Ordering::Acquire),
            self.inner.maximum.load(Ordering::Acquire),
        )
    }
}

pub(crate) struct CapacityPermit {
    _permit: OwnedSemaphorePermit,
    owner: Arc<AdmissionInner>,
}

pub(crate) struct SocketLease {
    _permit: CapacityPermit,
    _registration: SocketRegistration,
}

impl Drop for CapacityPermit {
    fn drop(&mut self) {
        self.owner.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Default)]
struct SocketRegistry {
    inner: Arc<SocketRegistryInner>,
}

#[derive(Default)]
struct SocketRegistryInner {
    entries: Mutex<HashMap<usize, Arc<SocketState>>>,
    next_id: AtomicUsize,
}

#[derive(Default)]
struct SocketState {
    closed: AtomicBool,
}

impl SocketRegistry {
    fn register(&self) -> SocketRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::AcqRel);
        let state = Arc::new(SocketState::default());
        self.inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, state.clone());
        SocketRegistration {
            owner: self.inner.clone(),
            state,
            id,
        }
    }

    fn active(&self) -> usize {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn force_close_all(&self) {
        let entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for state in entries.values() {
            state.closed.store(true, Ordering::Release);
        }
    }
}

struct SocketRegistration {
    owner: Arc<SocketRegistryInner>,
    state: Arc<SocketState>,
    id: usize,
}

impl SocketRegistration {
    fn check_open(&self) -> std::io::Result<()> {
        if self.state.closed.load(Ordering::Acquire) {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "proxy forced socket closure",
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        self.owner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// One admitted socket. Its capacity permit and registry entry follow it through pools and splits.
pub(crate) struct ManagedIo<S> {
    stream: S,
    _permit: CapacityPermit,
    registration: SocketRegistration,
}

impl<S> std::fmt::Debug for ManagedIo<S>
where
    S: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ManagedIo")
            .field(&self.stream)
            .finish()
    }
}

impl<S> ManagedIo<S> {
    fn new(stream: S, permit: CapacityPermit, registration: SocketRegistration) -> Self {
        Self {
            stream,
            _permit: permit,
            registration,
        }
    }
}

impl<S> AsyncRead for ManagedIo<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.registration.check_open() {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_read(context, buffer)
    }
}

impl<S> AsyncWrite for ManagedIo<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.registration.check_open() {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_write(context, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.registration.check_open() {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.registration.check_open() {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_shutdown(context)
    }
}
