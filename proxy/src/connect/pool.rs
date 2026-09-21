//! Bounded idle connection storage owned by the process supervisor.

use std::collections::HashMap;
use std::hash::Hash;
#[cfg(test)]
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const IDLE_POOL_CAPACITY: usize = 64;
pub(crate) const IDLE_POOL_LIFETIME: Duration = Duration::from_secs(60);

struct Entry<V> {
    value: V,
    idle_since: Instant,
    sequence: u64,
}
struct PoolState<K, V> {
    entries: HashMap<K, Entry<V>>,
    next_sequence: u64,
    closed: bool,
}
struct State<K, V> {
    pool: Mutex<PoolState<K, V>>,
    capacity: usize,
    lifetime: Duration,
}

/// A sealed per-connector pool. Cloning it only shares that connector's pool.
pub(crate) struct IdlePool<K, V> {
    state: Arc<State<K, V>>,
}
impl<K, V> Clone for IdlePool<K, V> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<K, V> IdlePool<K, V>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Send + 'static,
{
    pub(crate) fn new() -> Self {
        Self::build(IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME)
    }

    #[cfg(test)]
    pub(crate) fn with_limits(capacity: NonZeroUsize, lifetime: NonZeroDuration) -> Self {
        Self::build(capacity.get(), lifetime.get())
    }

    fn build(capacity: usize, lifetime: Duration) -> Self {
        let state = Arc::new(State {
            pool: Mutex::new(PoolState {
                entries: HashMap::new(),
                next_sequence: 0,
                closed: false,
            }),
            capacity,
            lifetime,
        });
        Self { state }
    }

    /// Removes a value only if it remains usable at the instant it is taken.
    pub(crate) fn take_if_reusable(&self, key: &K, reusable: impl FnOnce(&V) -> bool) -> Option<V> {
        let mut pool = self.lock();
        prune_locked(&mut pool, self.state.lifetime, Instant::now());
        if pool.closed {
            return None;
        }
        pool.entries
            .remove(key)
            .and_then(|entry| reusable(&entry.value).then_some(entry.value))
    }

    /// Stores a value only if it is currently reusable.
    pub(crate) fn put_if_reusable(&self, key: K, value: V, reusable: impl FnOnce(&V) -> bool) {
        if !reusable(&value) {
            return;
        }
        let state = &self.state;
        let mut pool = self.lock();
        prune_locked(&mut pool, state.lifetime, Instant::now());
        if pool.closed {
            return;
        }
        let sequence = pool.next_sequence;
        pool.next_sequence = pool.next_sequence.wrapping_add(1);
        pool.entries.insert(
            key,
            Entry {
                value,
                idle_since: Instant::now(),
                sequence,
            },
        );
        while pool.entries.len() > state.capacity {
            if let Some(oldest) = pool
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone())
            {
                pool.entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn prune(&self) {
        let mut pool = self.lock();
        prune_locked(&mut pool, self.state.lifetime, Instant::now());
    }

    pub(crate) fn close(&self) {
        let mut pool = self.lock();
        pool.closed = true;
        pool.entries.clear();
    }

    fn lock(&self) -> MutexGuard<'_, PoolState<K, V>> {
        self.state
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().entries.len()
    }
    #[cfg(test)]
    fn poison_for_test(&self) {
        let state = self.state.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = state.pool.lock().unwrap();
            panic!("test poison");
        });
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) struct NonZeroDuration(Duration);
#[cfg(test)]
impl NonZeroDuration {
    pub(crate) fn new(value: Duration) -> Option<Self> {
        (!value.is_zero()).then_some(Self(value))
    }
    fn get(self) -> Duration {
        self.0
    }
}

fn prune_locked<K, V>(pool: &mut PoolState<K, V>, lifetime: Duration, now: Instant) {
    pool.entries
        .retain(|_, entry| now.duration_since(entry.idle_since) < lifetime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    struct Value {
        live: bool,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn value(dropped: &Arc<AtomicUsize>) -> Value {
        Value {
            live: true,
            dropped: dropped.clone(),
        }
    }
    fn reusable(value: &Value) -> bool {
        value.live
    }
    #[tokio::test]
    async fn capacity_evicts_oldest_idle_key_deterministically() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_secs(1)).unwrap(),
        );
        pool.put_if_reusable("first", value(&dropped), reusable);
        pool.put_if_reusable("second", value(&dropped), reusable);
        pool.put_if_reusable("third", value(&dropped), reusable);
        assert!(pool.take_if_reusable(&"first", reusable).is_none());
        assert!(pool.take_if_reusable(&"second", reusable).is_some());
        assert!(pool.take_if_reusable(&"third", reusable).is_some());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }
    #[tokio::test]
    async fn replacement_and_dead_entries_release_their_values() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_secs(1)).unwrap(),
        );
        pool.put_if_reusable("key", value(&dropped), reusable);
        pool.put_if_reusable("key", value(&dropped), reusable);
        pool.put_if_reusable(
            "dead",
            Value {
                live: false,
                dropped: dropped.clone(),
            },
            reusable,
        );
        assert_eq!(pool.len(), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn poisoned_mutex_recovers_its_entries() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_secs(1)).unwrap(),
        );
        pool.put_if_reusable("key", value(&dropped), reusable);
        pool.poison_for_test();
        assert!(pool.take_if_reusable(&"key", reusable).is_some());
    }
    #[tokio::test]
    async fn supervisor_prune_expires_idle_values() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_millis(10)).unwrap(),
        );
        pool.put_if_reusable("key", value(&dropped), reusable);
        tokio::time::sleep(Duration::from_millis(11)).await;
        pool.prune();
        assert_eq!(pool.len(), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn shutdown_close_drops_entries_and_refuses_late_returns() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_secs(1)).unwrap(),
        );
        pool.put_if_reusable("first", value(&dropped), reusable);
        pool.close();
        pool.put_if_reusable("late", value(&dropped), reusable);
        assert_eq!(pool.len(), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pooled_upstream_permits_survive_idle_and_release_on_eviction_and_close() {
        let resources = crate::shutdown::ProcessResources::testing(8, 2);
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(1).unwrap(),
            NonZeroDuration::new(Duration::from_secs(60)).unwrap(),
        );
        let first = resources.manage_upstream((), resources.try_upstream().unwrap());
        pool.put_if_reusable("first", first, |_| true);
        assert_eq!(resources.upstream_counts(), (1, 1));

        let second = resources.manage_upstream((), resources.try_upstream().unwrap());
        pool.put_if_reusable("second", second, |_| true);
        assert_eq!(resources.upstream_counts(), (1, 2));
        assert!(pool.take_if_reusable(&"first", |_| true).is_none());

        let held = resources.try_upstream().expect("released eviction permit");
        assert!(resources.try_upstream().is_none());
        drop(held);
        pool.close();
        assert_eq!(resources.upstream_counts(), (0, 2));
    }

    #[tokio::test]
    async fn forced_registry_close_interrupts_tracked_socket_io() {
        let resources = crate::shutdown::ProcessResources::testing(1, 1);
        let (stream, _peer) = tokio::io::duplex(1);
        let mut stream = resources.manage_upstream(stream, resources.try_upstream().unwrap());
        assert_eq!(resources.registered_sockets(), 1);

        resources.force_close_all();
        let mut byte = [0_u8; 1];
        assert_eq!(
            stream.read(&mut byte).await.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
        drop(stream);
        assert_eq!(resources.registered_sockets(), 0);
        assert_eq!(resources.upstream_counts(), (0, 1));
    }

    #[test]
    fn production_upstream_admission_never_exceeds_two_hundred_fifty_six() {
        let resources = crate::shutdown::ProcessResources::production();
        let permits: Vec<_> = (0..crate::shutdown::MAX_UPSTREAM_CONNECTIONS)
            .map(|_| resources.try_upstream().expect("upstream permit"))
            .collect();
        assert!(resources.try_upstream().is_none());
        assert_eq!(
            resources.upstream_counts(),
            (
                crate::shutdown::MAX_UPSTREAM_CONNECTIONS,
                crate::shutdown::MAX_UPSTREAM_CONNECTIONS,
            )
        );
        drop(permits);
        assert_eq!(
            resources.upstream_counts(),
            (0, crate::shutdown::MAX_UPSTREAM_CONNECTIONS)
        );
    }
}
