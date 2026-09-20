//! A bounded in-memory cache.
//!
//! oxblocks is stateless: this is the only thing it remembers, it lives in the
//! process, and losing it costs latency rather than correctness.
//!
//! Hand-written rather than pulled in. The project's argument is a small
//! audited dependency tree, and what is needed here is roughly a hundred lines
//! with invariants a reader can check: bounded size, optional expiry,
//! least-recently-used eviction. Eviction scans the map, which is O(n) — a
//! correct scan beats an intrusive linked list nobody wants to audit — but the
//! scan clears a batch rather than one entry, so its cost is amortised over
//! thousands of inserts.
//!
//! ## What may be cached
//!
//! Confirmed blocks and transactions are immutable *once buried*, and that
//! qualifier is the whole reorg story. Entries keyed by **hash** are always
//! safe: a hash names one object forever. Entries keyed by **height** are not,
//! because a reorg reassigns a height to a different block — so callers must
//! not cache by height within [`REORG_WINDOW`] of the tip.
//!
//! This type does not enforce that; it cannot see heights. [`crate::rpc_source`]
//! does.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many blocks below the tip are treated as still reorganisable.
///
/// Monero reorgs are shallow — the deepest seen in practice is a handful of
/// blocks — but the cost of being wrong here is serving a block that no longer
/// exists, so the window is generous.
pub const REORG_WINDOW: u64 = 60;

/// Deep enough that being wrong needs an unprecedented reorg, shallow enough
/// that paging back through history stays warm.
///
/// A compile-time assertion rather than a test: a runtime check on a constant
/// can never fail, so it would buy confidence it does not provide. This fails
/// the build instead.
const _: () = assert!(
    REORG_WINDOW >= 20,
    "REORG_WINDOW must stay deep enough that a height is a stable name for a block"
);

/// Whether an object this far from the tip may be cached under a key that a
/// reorg could reassign — a height, as opposed to a hash.
///
/// Split out as a function so the rule can be tested directly. It is the one
/// place where caching could serve a block that no longer exists, and
/// "obviously right" is not the same as checked.
#[must_use]
pub const fn safe_to_cache_by_height(depth: u64) -> bool {
    depth >= REORG_WINDOW
}

/// What one eviction pass clears, as a fraction of capacity.
///
/// Evicting a single entry per insert means an O(n) scan per insert, and the
/// scan runs while holding the lock, so every other request waits behind it.
/// Measured on a release build, once the cache is full: 0.003 ms per insert at
/// 512 entries, 0.043 ms at 8,192, and **0.347 ms at the 65,536 the ring-member
/// cache uses**. One mainnet transaction with 195 inputs touches 3,120 ring
/// members, which is over a second of lock held for eviction alone.
///
/// Clearing a sixteenth of the cache in one pass spreads that scan over
/// thousands of inserts and leaves the eviction order unchanged: it is still
/// the least recently used entries that go, just several at a time.
const EVICT_FRACTION: usize = 16;

struct Entry<V> {
    value: Arc<V>,
    /// Logical clock reading, for least-recently-used ordering. A counter
    /// rather than a timestamp so that ordering does not depend on clock
    /// resolution or monotonicity.
    used_at: u64,
    stored_at: Instant,
}

struct Inner<K, V> {
    map: HashMap<K, Entry<V>>,
    clock: u64,
    hits: u64,
    misses: u64,
}

pub struct Cache<K, V> {
    inner: Mutex<Inner<K, V>>,
    capacity: usize,
    ttl: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub len: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
}

impl<K: Eq + Hash + Clone, V> Cache<K, V> {
    /// A cache holding at most `capacity` entries forever.
    ///
    /// For immutable objects: a block that will never change does not need to
    /// be re-fetched on a timer.
    #[must_use]
    pub fn permanent(capacity: usize) -> Self {
        Self::new(capacity, None)
    }

    /// A cache whose entries expire after `ttl`.
    ///
    /// For values that track the chain tip — the height, the mempool, the fee
    /// estimate — where being a few seconds stale is fine and being a minute
    /// stale is not.
    #[must_use]
    pub fn expiring(capacity: usize, ttl: Duration) -> Self {
        Self::new(capacity, Some(ttl))
    }

    fn new(capacity: usize, ttl: Option<Duration>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                clock: 0,
                hits: 0,
                misses: 0,
            }),
            // A zero-capacity cache would evict what it just inserted and then
            // return it, which reads as a cache that never works. Treat it as
            // one entry instead.
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// Fetch, if present and unexpired.
    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        let Ok(mut inner) = self.inner.lock() else {
            // A poisoned lock means another thread panicked while holding it.
            // The cache is not worth propagating that: report a miss and let
            // the caller do the real work.
            return None;
        };

        if let Some(ttl) = self.ttl
            && inner
                .map
                .get(key)
                .is_some_and(|e| e.stored_at.elapsed() >= ttl)
        {
            inner.map.remove(key);
        }

        inner.clock += 1;
        let clock = inner.clock;
        match inner.map.get_mut(key) {
            Some(entry) => {
                entry.used_at = clock;
                let value = Arc::clone(&entry.value);
                inner.hits += 1;
                Some(value)
            }
            None => {
                inner.misses += 1;
                None
            }
        }
    }

    /// Drop the least recently used `capacity / EVICT_FRACTION` entries.
    ///
    /// One scan collects the ages, a linear selection finds the cut-off, and a
    /// second pass removes everything at or below it. `used_at` comes from a
    /// counter that increments on every access, so no two entries share a
    /// value and the cut is exact.
    fn evict_batch(inner: &mut Inner<K, V>, capacity: usize) {
        let drop_count = (capacity / EVICT_FRACTION).max(1);
        let mut ages: Vec<u64> = inner.map.values().map(|e| e.used_at).collect();
        if drop_count >= ages.len() {
            inner.map.clear();
            return;
        }
        // `select_nth_unstable(n)` puts the (n+1)-th smallest age at index n,
        // so exactly `drop_count` entries sit strictly below it. Keeping that
        // entry is what makes the count exact rather than one too many.
        let (_, cutoff, _) = ages.select_nth_unstable(drop_count);
        let cutoff = *cutoff;
        inner.map.retain(|_, e| e.used_at >= cutoff);
    }

    /// Store, evicting a batch of the least recently used entries if that
    /// would exceed capacity. Returns the stored value so a caller can use it
    /// without a second lookup.
    pub fn insert(&self, key: K, value: V) -> Arc<V> {
        let value = Arc::new(value);
        let Ok(mut inner) = self.inner.lock() else {
            return value;
        };

        inner.clock += 1;
        let clock = inner.clock;

        if !inner.map.contains_key(&key) && inner.map.len() >= self.capacity {
            Self::evict_batch(&mut inner, self.capacity);
        }

        inner.map.insert(
            key,
            Entry {
                value: Arc::clone(&value),
                used_at: clock,
                stored_at: Instant::now(),
            },
        );
        value
    }

    pub fn stats(&self) -> Stats {
        self.inner.lock().map_or(
            Stats {
                len: 0,
                capacity: self.capacity,
                hits: 0,
                misses: 0,
            },
            |i| Stats {
                len: i.map.len(),
                capacity: self.capacity,
                hits: i.hits,
                misses: i.misses,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;

    #[test]
    fn only_buried_blocks_may_be_cached_by_height() {
        // The tip and everything near it can still be reorganised onto a
        // different block, so a height is not yet a stable name for one.
        assert!(!safe_to_cache_by_height(0));
        assert!(!safe_to_cache_by_height(1));
        assert!(!safe_to_cache_by_height(REORG_WINDOW - 1));
        // At and beyond the window, a height names one block in practice.
        assert!(safe_to_cache_by_height(REORG_WINDOW));
        assert!(safe_to_cache_by_height(1_000_000));
    }

    #[test]
    fn stores_and_returns_a_value() {
        let c: Cache<u64, String> = Cache::permanent(4);
        assert!(c.get(&1).is_none());
        c.insert(1, "one".to_owned());
        assert_eq!(*c.get(&1).unwrap(), "one");
        let s = c.stats();
        assert_eq!((s.hits, s.misses, s.len), (1, 1, 1));
    }

    /// Eviction clears a batch, and the batch is exactly the size it claims.
    ///
    /// The cost of getting this wrong is not a wrong answer -- an evicted
    /// entry is refetched -- but a cache that quietly holds a fraction of what
    /// it was sized for, or one that goes back to scanning per insert. Both
    /// are invisible without counting.
    #[test]
    fn eviction_clears_a_batch_of_the_least_recently_used() {
        const CAPACITY: usize = 64;
        let expected_drop = CAPACITY / EVICT_FRACTION;
        assert!(expected_drop > 1, "the batch must be bigger than one entry");

        let c: Cache<u64, u64> = Cache::permanent(CAPACITY);
        for i in 0..CAPACITY as u64 {
            c.insert(i, i);
        }
        assert_eq!(c.stats().len, CAPACITY);

        // Touch the oldest half, so recency and insertion order disagree.
        for i in 0..(CAPACITY as u64 / 2) {
            assert!(c.get(&i).is_some());
        }

        c.insert(1000, 1000);
        assert_eq!(
            c.stats().len,
            CAPACITY - expected_drop + 1,
            "one pass should drop exactly {expected_drop} entries"
        );

        // The victims are the untouched ones, which are now least recent.
        let gone = (0..CAPACITY as u64).filter(|i| c.get(i).is_none()).count();
        assert_eq!(gone, expected_drop);
        for i in 0..(CAPACITY as u64 / 2) {
            assert!(c.get(&i).is_some(), "{i} was touched and must survive");
        }
    }

    #[test]
    fn evicts_the_least_recently_used_entry_not_the_oldest() {
        let c: Cache<u64, u64> = Cache::permanent(3);
        c.insert(1, 10);
        c.insert(2, 20);
        c.insert(3, 30);

        // Touch 1, making 2 the least recently used even though 1 is older.
        assert_eq!(*c.get(&1).unwrap(), 10);
        c.insert(4, 40);

        assert!(c.get(&2).is_none(), "2 was least recently used");
        assert!(c.get(&1).is_some(), "1 was touched and must survive");
        assert!(c.get(&3).is_some());
        assert!(c.get(&4).is_some());
        assert_eq!(c.stats().len, 3);
    }

    #[test]
    fn never_grows_past_capacity() {
        let c: Cache<u64, u64> = Cache::permanent(8);
        for i in 0..1000 {
            c.insert(i, i);
        }
        assert_eq!(c.stats().len, 8);
    }

    /// Re-inserting an existing key must not evict a different entry to make
    /// room for something already present.
    #[test]
    fn overwriting_an_existing_key_does_not_evict() {
        let c: Cache<u64, u64> = Cache::permanent(2);
        c.insert(1, 10);
        c.insert(2, 20);
        c.insert(1, 11);
        assert_eq!(c.stats().len, 2);
        assert_eq!(*c.get(&1).unwrap(), 11);
        assert_eq!(*c.get(&2).unwrap(), 20);
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        let c: Cache<u64, u64> = Cache::expiring(4, Duration::from_millis(40));
        c.insert(1, 10);
        assert!(c.get(&1).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.get(&1).is_none(), "the entry outlived its ttl");
        assert_eq!(c.stats().len, 0, "and is dropped, not merely hidden");
    }

    #[test]
    fn a_permanent_entry_does_not_expire() {
        let c: Cache<u64, u64> = Cache::permanent(4);
        c.insert(1, 10);
        std::thread::sleep(Duration::from_millis(30));
        assert!(c.get(&1).is_some());
    }

    /// A zero capacity would otherwise evict the entry being inserted, making
    /// every lookup a miss on a cache that looks configured.
    #[test]
    fn zero_capacity_is_treated_as_one() {
        let c: Cache<u64, u64> = Cache::permanent(0);
        c.insert(1, 10);
        assert_eq!(*c.get(&1).unwrap(), 10);
    }

    #[test]
    fn values_are_shared_not_copied() {
        let c: Cache<u64, Vec<u8>> = Cache::permanent(2);
        let stored = c.insert(1, vec![1, 2, 3]);
        let fetched = c.get(&1).unwrap();
        assert!(
            Arc::ptr_eq(&stored, &fetched),
            "a cache hit must not clone the value"
        );
    }

    #[test]
    fn is_usable_from_several_threads() {
        let c: Arc<Cache<u64, u64>> = Arc::new(Cache::permanent(64));
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let c = Arc::clone(&c);
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    c.insert(t * 1000 + i, i);
                    let _ = c.get(&(t * 1000 + i));
                }
            }));
        }
        for h in handles {
            h.join().expect("no thread panicked");
        }
        assert!(c.stats().len <= 64);
    }
}
