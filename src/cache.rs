use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

/// Hotspots remembered per packet, enough for any sane multibuy max. Beyond
/// this, further copies are still counted; they just can't be matched to an
/// earlier hotspot.
const MAX_HOTSPOTS_PER_KEY: usize = 16;

#[derive(Debug, Clone)]
pub(crate) struct CacheValue {
    count: u32,
    pub(crate) created_at: Instant,
    /// Hashes of the hotspot keys that sent copies of this packet, so a later
    /// copy can be told apart as the same hotspot again (a device resend) or a
    /// different one arriving late.
    hotspots: Vec<u64>,
}

/// What the cache knew about a packet when one more copy arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    /// Copies counted so far, including this one.
    pub count: u32,
    /// How long ago the first copy arrived; `None` if this is the first.
    pub since_first: Option<Duration>,
    /// Whether this hotspot had already sent a copy of this packet.
    pub same_hotspot: bool,
}

fn hotspot_hash(hotspot_key: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hotspot_key.hash(&mut hasher);
    hasher.finish()
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

    /// Count one copy of packet `key`, heard by `hotspot_key`.
    ///
    /// An empty `hotspot_key` (a client that doesn't send one) is never
    /// matched as the same hotspot.
    pub fn inc(&self, key: String, hotspot_key: &[u8]) -> Seen {
        let hotspot = (!hotspot_key.is_empty()).then(|| hotspot_hash(hotspot_key));
        match self.map.entry(key) {
            Entry::Occupied(mut entry) => {
                let val = entry.get_mut();
                val.count += 1;
                let same_hotspot = hotspot.is_some_and(|h| val.hotspots.contains(&h));
                if let Some(h) = hotspot {
                    if !same_hotspot && val.hotspots.len() < MAX_HOTSPOTS_PER_KEY {
                        val.hotspots.push(h);
                    }
                }
                Seen {
                    count: val.count,
                    since_first: Some(val.created_at.elapsed()),
                    same_hotspot,
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(CacheValue {
                    count: 1,
                    created_at: Instant::now(),
                    hotspots: hotspot.into_iter().collect(),
                });
                crate::metrics::inc_cache_size();
                Seen {
                    count: 1,
                    since_first: None,
                    same_hotspot: false,
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_a_known_hotspot_from_a_new_one() {
        let cache = Cache::new();
        let first = cache.inc("pkt".into(), b"hs-a");
        assert_eq!(first.count, 1);
        assert!(first.since_first.is_none());

        assert!(!cache.inc("pkt".into(), b"hs-b").same_hotspot);
        assert!(cache.inc("pkt".into(), b"hs-a").same_hotspot);
        assert!(cache.inc("pkt".into(), b"hs-b").same_hotspot);
        // Same hotspot, different packet.
        assert!(!cache.inc("other".into(), b"hs-a").same_hotspot);
    }

    #[test]
    fn empty_hotspot_key_never_matches() {
        let cache = Cache::new();
        cache.inc("pkt".into(), b"");
        let again = cache.inc("pkt".into(), b"");
        assert_eq!(again.count, 2);
        assert!(!again.same_hotspot);
    }
}
