//! Bounded, self-reaping idle connection storage.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub(crate) const IDLE_POOL_CAPACITY: usize = 64;
pub(crate) const IDLE_POOL_LIFETIME: Duration = Duration::from_secs(60);

pub(crate) trait IdleValue {
    fn reusable(&self) -> bool;
}

struct Entry<V> {
    value: V,
    idle_since: Instant,
    sequence: u64,
}

struct State<K, V> {
    entries: Mutex<HashMap<K, Entry<V>>>,
    next_sequence: Mutex<u64>,
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
    V: IdleValue + Send + 'static,
{
    pub(crate) fn new(capacity: usize, lifetime: Duration) -> Self {
        assert!(capacity != 0, "idle pool capacity must be nonzero");
        assert!(!lifetime.is_zero(), "idle pool lifetime must be nonzero");
        let state = Arc::new(State {
            entries: Mutex::new(HashMap::new()),
            next_sequence: Mutex::new(0),
            changed: Notify::new(),
            capacity,
            lifetime,
        });
        let reaper = tokio::spawn(reap(Arc::downgrade(&state)));
        Self {
            owner: Arc::new(Owner { state, reaper }),
        }
    }

    pub(crate) fn take(&self, key: &K) -> Option<V> {
        self.prune();
        self.owner
            .state
            .entries
            .lock()
            .ok()
            .and_then(|mut entries| entries.remove(key))
            .map(|entry| entry.value)
            .filter(IdleValue::reusable)
    }

    pub(crate) fn put(&self, key: K, value: V) {
        if !value.reusable() {
            return;
        }
        self.prune();
        let state = &self.owner.state;
        if let Ok(mut entries) = state.entries.lock() {
            let sequence = next_sequence(&state.next_sequence);
            entries.insert(
                key,
                Entry {
                    value,
                    idle_since: Instant::now(),
                    sequence,
                },
            );
            while entries.len() > state.capacity {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.sequence)
                    .map(|(key, _)| key.clone())
                    .expect("pool exceeds zero capacity");
                entries.remove(&oldest);
            }
        }
        state.changed.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.owner
            .state
            .entries
            .lock()
            .map_or(0, |entries| entries.len())
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        if let Ok(mut entries) = self.owner.state.entries.lock() {
            entries.clear();
        }
        self.owner.state.changed.notify_one();
    }

    fn prune(&self) {
        prune_state(&self.owner.state);
    }
}

fn next_sequence(sequence: &Mutex<u64>) -> u64 {
    let mut sequence = sequence.lock().expect("idle pool sequence lock poisoned");
    let current = *sequence;
    *sequence = sequence.wrapping_add(1);
    current
}

fn prune_state<K, V>(state: &State<K, V>)
where
    K: Eq + Hash,
    V: IdleValue,
{
    let now = Instant::now();
    if let Ok(mut entries) = state.entries.lock() {
        entries.retain(|_, entry| {
            entry.value.reusable() && now.duration_since(entry.idle_since) < state.lifetime
        });
    }
}

async fn reap<K, V>(state: Weak<State<K, V>>)
where
    K: Eq + Hash + Send + 'static,
    V: IdleValue + Send + 'static,
{
    while let Some(state) = state.upgrade() {
        let next = state.entries.lock().ok().and_then(|entries| {
            entries
                .values()
                .map(|entry| entry.idle_since + state.lifetime)
                .min()
        });
        match next {
            Some(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.into()) => prune_state(&state),
                    () = state.changed.notified() => {},
                }
            }
            None => state.changed.notified().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Value {
        live: bool,
        dropped: Arc<AtomicUsize>,
    }

    impl IdleValue for Value {
        fn reusable(&self) -> bool {
            self.live
        }
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

    #[tokio::test]
    async fn capacity_evicts_oldest_idle_key_deterministically() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::new(2, Duration::from_secs(1));
        pool.put("first", value(&dropped));
        pool.put("second", value(&dropped));
        pool.put("third", value(&dropped));
        assert_eq!(pool.len(), 2);
        assert!(pool.take(&"first").is_none());
        assert!(pool.take(&"second").is_some());
        assert!(pool.take(&"third").is_some());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn replacement_and_dead_entries_release_their_values() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::new(2, Duration::from_secs(1));
        pool.put("key", value(&dropped));
        pool.put("key", value(&dropped));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        pool.put(
            "dead",
            Value {
                live: false,
                dropped: dropped.clone(),
            },
        );
        assert_eq!(pool.len(), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reaper_expires_idle_values_without_another_pool_operation() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let pool = IdlePool::new(2, Duration::from_millis(10));
        pool.put("key", value(&dropped));
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
