use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::time::Instant;

#[derive(Debug, Clone)]
pub(crate) struct CacheValue {
    count: u32,
    pub(crate) created_at: Instant,
}

#[derive(Default)]
pub struct Cache {
    map: DashMap<String, CacheValue>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            map: DashMap::new(),
        }
    }

    /// Count one request for `key`. Returns the new count and, for every
    /// request after the first, how long ago the first one arrived.
    pub fn inc(&self, key: String) -> (u32, Option<std::time::Duration>) {
        match self.map.entry(key) {
            Entry::Occupied(mut entry) => {
                let val = entry.get_mut();
                val.count += 1;
                (val.count, Some(val.created_at.elapsed()))
            }
            Entry::Vacant(entry) => {
                entry.insert(CacheValue {
                    count: 1,
                    created_at: Instant::now(),
                });
                crate::metrics::inc_cache_size();
                (1, None)
            }
        }
    }

    pub fn remove_expired(&self, max_age: std::time::Duration) -> usize {
        let before = self.map.len();
        self.map.retain(|_, v| v.created_at.elapsed() < max_age);
        let after = self.map.len();
        let removed = before - after;
        crate::metrics::set_cache_size(after as f64);
        removed
    }
}
