//! Per-storage metadata cache with time-stamped directory listings.
//!
//! The cache exists to make recursive walks cheap within one CLI run and to
//! give future front-ends (NFS mount) a consistent view between refreshes.
//! Mutating operations invalidate the affected parent listing immediately.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::model::Entry;

/// How long a directory listing stays fresh.
pub const LIST_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct StorageCache {
    entries: HashMap<u64, Entry>,
    listings: HashMap<u64, Listing>,
}

#[derive(Debug)]
struct Listing {
    fetched_at: Instant,
    children: Vec<Entry>,
}

/// Metadata cache keyed by storage id, then object handle.
#[derive(Debug, Default)]
pub struct MetaCache {
    storages: HashMap<u32, StorageCache>,
}

impl MetaCache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&mut self, storage_id: u32) -> &mut StorageCache {
        self.storages.entry(storage_id).or_default()
    }

    /// Returns a cached listing if it is younger than [`LIST_TTL`].
    #[must_use]
    pub fn listing(&self, storage_id: u32, dir: u64) -> Option<&[Entry]> {
        let st = self.storages.get(&storage_id)?;
        let l = st.listings.get(&dir)?;
        if l.fetched_at.elapsed() > LIST_TTL {
            return None;
        }
        Some(&l.children)
    }

    /// Stores a fresh listing for `dir` and indexes every child entry.
    pub fn store_listing(&mut self, storage_id: u32, dir: u64, children: Vec<Entry>) {
        let st = self.slot(storage_id);
        for e in &children {
            st.entries.insert(e.handle, e.clone());
        }
        st.listings.insert(
            dir,
            Listing {
                fetched_at: Instant::now(),
                children,
            },
        );
    }

    /// Drops the cached listing of `parent` (after create/delete/rename/move)
    /// and removes `handle` from the entry index when given.
    pub fn invalidate(&mut self, storage_id: u32, parent: u64, handle: Option<u64>) {
        let st = self.slot(storage_id);
        st.listings.remove(&parent);
        if let Some(h) = handle {
            st.entries.remove(&h);
        }
    }

    /// Drops every cached fact about a storage.
    pub fn clear_storage(&mut self, storage_id: u32) {
        self.storages.remove(&storage_id);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn entry(handle: u64, name: &str) -> Entry {
        Entry {
            handle,
            parent: 0,
            name: name.to_string(),
            is_dir: false,
            size: 1,
        }
    }

    #[test]
    fn stores_and_returns_listing() {
        let mut c = MetaCache::new();
        c.store_listing(7, 0, vec![entry(1, "a.txt"), entry(2, "b.txt")]);
        let got = c.listing(7, 0).expect("listing miss");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "a.txt");
    }

    #[test]
    fn invalidation_drops_parent_and_entry() {
        let mut c = MetaCache::new();
        c.store_listing(7, 0, vec![entry(1, "a.txt")]);
        c.invalidate(7, 0, Some(1));
        assert!(c.listing(7, 0).is_none());
    }

    #[test]
    fn unknown_storage_is_a_miss() {
        let c = MetaCache::new();
        assert!(c.listing(42, 0).is_none());
    }
}
