//! Bounded, self-reaping idle connection storage.

use std::collections::HashMap;
use std::hash::Hash;
#[cfg(test)]
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

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
}
struct State<K, V> {
    pool: Mutex<PoolState<K, V>>,
    changed: Notify,
    capacity: usize,
    lifetime: Duration,
}
struct Owner<K, V> {
    state: Arc<State<K, V>>,
    reaper: JoinHandle<()>,
}
impl<K, V> Drop for Owner<K, V> {
    fn drop(&mut self) {
        self.reaper.abort();
    }
}

/// A sealed per-connector pool. Cloning it only shares that connector's pool.
pub(crate) struct IdlePool<K, V> {
    owner: Arc<Owner<K, V>>,
}
impl<K, V> Clone for IdlePool<K, V> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
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
            }),
            changed: Notify::new(),
            capacity,
            lifetime,
        });
        let reaper = tokio::spawn(reap(Arc::downgrade(&state)));
        Self {
            owner: Arc::new(Owner { state, reaper }),
        }
    }

    /// Removes a value only if it remains usable at the instant it is taken.
    pub(crate) fn take_if_reusable(&self, key: &K, reusable: impl FnOnce(&V) -> bool) -> Option<V> {
        let mut pool = self.lock();
        prune_locked(&mut pool, self.owner.state.lifetime, Instant::now());
        pool.entries
            .remove(key)
            .and_then(|entry| reusable(&entry.value).then_some(entry.value))
    }

    /// Stores a value only if it is currently reusable.
    pub(crate) fn put_if_reusable(&self, key: K, value: V, reusable: impl FnOnce(&V) -> bool) {
        if !reusable(&value) {
            return;
        }
        let state = &self.owner.state;
        let mut pool = self.lock();
        prune_locked(&mut pool, state.lifetime, Instant::now());
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
        drop(pool);
        state.changed.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, PoolState<K, V>> {
        self.owner
            .state
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
        let state = self.owner.state.clone();
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

async fn reap<K, V>(state: Weak<State<K, V>>)
where
    K: Eq + Hash + Send + 'static,
    V: Send + 'static,
{
    while let Some(state) = state.upgrade() {
        let next = {
            let pool = state
                .pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pool.entries
                .values()
                .map(|entry| entry.idle_since + state.lifetime)
                .min()
        };
        match next {
            Some(deadline) => tokio::select! {
                () = tokio::time::sleep_until(deadline.into()) => { let mut pool = state.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner); prune_locked(&mut pool, state.lifetime, Instant::now()); }
                () = state.changed.notified() => {}
            },
            None => state.changed.notified().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    async fn reaper_expires_idle_values_without_another_pool_operation() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::with_limits(
            NonZeroUsize::new(2).unwrap(),
            NonZeroDuration::new(Duration::from_millis(10)).unwrap(),
        );
        pool.put_if_reusable("key", value(&dropped), reusable);
        tokio::time::timeout(Duration::from_millis(200), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pool.len(), 0);
    }
}
