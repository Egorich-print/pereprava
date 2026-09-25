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

/// Upper bound on live listings kept per storage. Expired entries are pruned
/// on every store; this cap protects a long-lived process from unbounded
/// growth when many distinct directories are walked.
const MAX_LISTINGS: usize = 256;

#[derive(Debug, Default)]
struct StorageCache {
    listings: HashMap<u64, Listing>,
    /// Child handle -> id of the directory it was listed from.
    ///
    /// Learned from the listing key rather than from `Entry::parent`, which is
    /// the device's own claim and is not necessarily the directory we asked
    /// about. A handle-based delete has no parent in hand, and guessing one
    /// left stale listings visible for up to [`LIST_TTL`] after every delete.
    parents: HashMap<u64, u64>,
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

    /// Stores a fresh listing for `dir`, pruning stale entries first.
    pub fn store_listing(&mut self, storage_id: u32, dir: u64, children: Vec<Entry>) {
        let st = self.slot(storage_id);
        st.listings
            .retain(|_, l| l.fetched_at.elapsed() <= LIST_TTL);
        if st.listings.len() >= MAX_LISTINGS
            && !st.listings.contains_key(&dir)
            && let Some(oldest) = st
                .listings
                .iter()
                .min_by_key(|(_, l)| l.fetched_at)
                .map(|(k, _)| *k)
        {
            st.listings.remove(&oldest);
        }
        for child in &children {
            st.parents.insert(child.handle, dir);
        }
        st.listings.insert(
            dir,
            Listing {
                fetched_at: Instant::now(),
                children,
            },
        );
    }

    /// The directory `handle` was last seen in, if a listing revealed it.
    #[must_use]
    pub fn parent_of(&self, storage_id: u32, handle: u64) -> Option<u64> {
        self.storages
            .get(&storage_id)?
            .parents
            .get(&handle)
            .copied()
    }

    /// Drops the cached listing of `parent` (after create/delete/rename/move).
    pub fn invalidate(&mut self, storage_id: u32, parent: u64) {
        self.slot(storage_id).listings.remove(&parent);
    }

    /// Invalidates the directory that actually held `handle`.
    ///
    /// Used by handle-based operations, which know the object but not where it
    /// lives. When no listing ever revealed the parent, the only safe move is
    /// to drop the whole storage: any of its directories may be the stale one.
    pub fn invalidate_handle(&mut self, storage_id: u32, handle: u64) {
        match self.parent_of(storage_id, handle) {
            Some(parent) => self.invalidate(storage_id, parent),
            None => self.clear_storage(storage_id),
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
    fn invalidation_drops_parent_listing() {
        let mut c = MetaCache::new();
        c.store_listing(7, 0, vec![entry(1, "a.txt")]);
        c.invalidate(7, 0);
        assert!(c.listing(7, 0).is_none());
    }

    #[test]
    fn handle_delete_invalidates_the_real_parent() {
        let mut c = MetaCache::new();
        c.store_listing(7, 0, vec![entry(10, "DCIM")]);
        c.store_listing(7, 10, vec![entry(20, "a.jpg"), entry(21, "b.jpg")]);
        c.store_listing(7, 20, vec![entry(30, "c.jpg")]);

        // Deleting a.jpg must drop DCIM's listing, not the storage root.
        c.invalidate_handle(7, 20);
        assert!(
            c.listing(7, 10).is_none(),
            "the real parent must be dropped"
        );
        assert!(
            c.listing(7, 0).is_some(),
            "unrelated directories must survive"
        );
        assert!(c.listing(7, 20).is_some(), "a child's listing is unrelated");
    }

    #[test]
    fn unknown_handle_falls_back_to_dropping_the_storage() {
        let mut c = MetaCache::new();
        c.store_listing(7, 0, vec![entry(10, "DCIM")]);
        // No listing ever revealed handle 99, so any directory could hold it.
        c.invalidate_handle(7, 99);
        assert!(c.listing(7, 0).is_none());
    }

    #[test]
    fn parent_is_learned_from_the_listing_key() {
        // The device's own `parent` field is deliberately wrong here: the
        // directory we listed from is the fact we cache.
        let mut c = MetaCache::new();
        let mut e = entry(20, "a.jpg");
        e.parent = 999;
        c.store_listing(7, 10, vec![e]);
        assert_eq!(c.parent_of(7, 20), Some(10));
    }

    #[test]
    fn unknown_storage_is_a_miss() {
        let c = MetaCache::new();
        assert!(c.listing(42, 0).is_none());
    }
}
