//! Every hotspot that has sent this service a copy, with running stats,
//! persisted across restarts.
//!
//! The request path only bumps atomic counters on an existing entry (or inserts
//! one the first time a hotspot is seen). A background task
//! ([`crate::tasks::hotspot_saver`]) writes the registry to disk every
//! [`SAVE_INTERVAL`] and on shutdown, and drops hotspots not seen for
//! [`RETENTION`] so the file stays bounded by the hotspots actually in use.

use crate::cache::Seen;
use crate::deny_lists;
use crate::traffic::{now_unix, Arrival, Ring};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How often the registry is written to disk (when it has changed).
pub const SAVE_INTERVAL: Duration = Duration::from_secs(60);

/// Hotspots not seen for this long are dropped at the next save.
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Most hotspots kept. One operator's devices are heard by tens to hundreds;
/// the cap only guards memory and file size if something sends junk keys.
const MAX_HOTSPOTS: usize = 20_000;

const FORMAT_VERSION: u32 = 1;

/// Region bit for enum values that don't fit the 64-bit mask (e.g. UNKNOWN=99).
const OTHER_REGION_BIT: u32 = 63;

fn region_bit(region: i32) -> u64 {
    let bit = u32::try_from(region)
        .ok()
        .filter(|&v| v < OTHER_REGION_BIT)
        .unwrap_or(OTHER_REGION_BIT);
    1 << bit
}

fn region_names(mask: u64) -> Vec<String> {
    (0..=OTHER_REGION_BIT)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| {
            if bit == OTHER_REGION_BIT {
                "other".to_string()
            } else {
                deny_lists::region_label(bit as i32)
            }
        })
        .collect()
}

/// Running stats for one hotspot. All counters are since the hotspot was first
/// seen (persisted), except `last_hour`, which is in memory only.
pub struct HotspotStats {
    copies: AtomicU64,
    /// Arrival position among copies of the same packet.
    first: AtomicU64,
    second: AtomicU64,
    later: AtomicU64,
    late: AtomicU64,
    slow: AtomicU64,
    resends: AtomicU64,
    denied: AtomicU64,
    /// Sum and count of delays behind the first copy, for copies under
    /// [`crate::traffic::REPEAT_AFTER`], to give a mean.
    delay_ms_sum: AtomicU64,
    delay_count: AtomicU64,
    /// Bitmask of region enum values seen.
    regions: AtomicU64,
    first_seen: AtomicU64,
    last_seen: AtomicU64,
    /// Unix seconds of the last late, slow or resent copy.
    last_late: AtomicU64,
    /// Copies per minute over the last hour.
    last_hour: Ring,
    /// Animal name, derived on first read rather than on the request path.
    name: OnceLock<String>,
}

impl HotspotStats {
    fn new(now: u64) -> Self {
        Self::from_saved(
            &SavedHotspot {
                first_seen: now,
                last_seen: now,
                ..Default::default()
            },
            now,
        )
    }

    fn from_saved(saved: &SavedHotspot, now: u64) -> Self {
        Self {
            copies: AtomicU64::new(saved.copies),
            first: AtomicU64::new(saved.first),
            second: AtomicU64::new(saved.second),
            later: AtomicU64::new(saved.later),
            late: AtomicU64::new(saved.late),
            slow: AtomicU64::new(saved.slow),
            resends: AtomicU64::new(saved.resends),
            denied: AtomicU64::new(saved.denied),
            delay_ms_sum: AtomicU64::new(saved.delay_ms_sum),
            delay_count: AtomicU64::new(saved.delay_count),
            regions: AtomicU64::new(saved.regions),
            first_seen: AtomicU64::new(saved.first_seen),
            last_seen: AtomicU64::new(saved.last_seen),
            last_late: AtomicU64::new(saved.last_late),
            last_hour: Ring::with_len(60, now / 60),
            name: OnceLock::new(),
        }
    }

    fn to_saved(&self) -> SavedHotspot {
        SavedHotspot {
            copies: self.copies.load(Relaxed),
            first: self.first.load(Relaxed),
            second: self.second.load(Relaxed),
            later: self.later.load(Relaxed),
            late: self.late.load(Relaxed),
            slow: self.slow.load(Relaxed),
            resends: self.resends.load(Relaxed),
            denied: self.denied.load(Relaxed),
            delay_ms_sum: self.delay_ms_sum.load(Relaxed),
            delay_count: self.delay_count.load(Relaxed),
            regions: self.regions.load(Relaxed),
            first_seen: self.first_seen.load(Relaxed),
            last_seen: self.last_seen.load(Relaxed),
            last_late: self.last_late.load(Relaxed),
        }
    }
}

/// One hotspot's stats as the API returns them.
#[derive(Debug, Clone, Serialize)]
pub struct HotspotView {
    pub address: String,
    pub name: String,
    pub copies: u64,
    pub copies_last_hour: u64,
    pub first: u64,
    pub second: u64,
    pub later: u64,
    pub late: u64,
    pub slow: u64,
    pub resends: u64,
    pub denied: u64,
    /// Mean delay behind the first copy of a packet, for this hotspot's copies
    /// that weren't first. `None` if it has always been first.
    pub mean_delay_ms: Option<f64>,
    pub regions: Vec<String>,
    pub first_seen: u64,
    pub last_seen: u64,
    /// Unix seconds of the last late, slow or resent copy; 0 if none.
    pub last_late: u64,
}

/// The registry of hotspots.
pub struct Hotspots {
    map: DashMap<String, HotspotStats>,
    store: HotspotStore,
    /// Set on every change, cleared by a save, so an idle service doesn't
    /// rewrite an unchanged file every minute.
    dirty: AtomicBool,
}

impl Hotspots {
    /// Load the registry from `store`. A missing file starts empty; a bad one
    /// is moved aside and reported, and the registry starts empty — hotspot
    /// stats aren't worth keeping multibuy offline for.
    pub fn load(store: HotspotStore) -> Self {
        let now = now_unix();
        let saved = match store.load() {
            Ok(saved) => saved,
            Err(e) => {
                tracing::error!("could not read the hotspot store: {e}");
                if let Some(path) = store.quarantine() {
                    tracing::warn!(
                        "moved the unreadable hotspot store to {} and started empty",
                        path.display()
                    );
                }
                BTreeMap::new()
            }
        };
        let map: DashMap<String, HotspotStats> = saved
            .iter()
            .map(|(address, s)| (address.clone(), HotspotStats::from_saved(s, now)))
            .collect();

        match store.path() {
            Some(path) => tracing::info!(
                hotspots = map.len(),
                "hotspot stats persist to {}",
                path.display()
            ),
            None => tracing::warn!("hotspot_store is empty; hotspot stats will be lost on restart"),
        }

        Self {
            map,
            store,
            dirty: AtomicBool::new(false),
        }
    }

    /// An in-memory registry, for tests.
    pub fn in_memory() -> Self {
        Self::load(HotspotStore::disabled())
    }

    pub fn is_persistent(&self) -> bool {
        self.store.path().is_some()
    }

    pub fn store_path(&self) -> Option<&Path> {
        self.store.path()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Record one copy from `hotspot` (its b58 address).
    pub fn record(&self, hotspot: &str, seen: &Seen, arrival: Arrival, region: i32, denied: bool) {
        if hotspot.is_empty() {
            return;
        }
        let now = now_unix();
        if let Some(stats) = self.map.get(hotspot) {
            Self::bump(&stats, seen, arrival, region, denied, now);
        } else if self.map.len() < MAX_HOTSPOTS {
            let stats = self
                .map
                .entry(hotspot.to_string())
                .or_insert_with(|| HotspotStats::new(now));
            Self::bump(&stats, seen, arrival, region, denied, now);
        } else {
            return;
        }
        self.dirty.store(true, Relaxed);
    }

    fn bump(
        stats: &HotspotStats,
        seen: &Seen,
        arrival: Arrival,
        region: i32,
        denied: bool,
        now: u64,
    ) {
        stats.copies.fetch_add(1, Relaxed);
        stats.last_seen.store(now, Relaxed);
        stats.last_hour.record_at(now / 60);
        stats.regions.fetch_or(region_bit(region), Relaxed);
        if denied {
            stats.denied.fetch_add(1, Relaxed);
        }

        // A resend is the device transmitting again, not another position in
        // the race between hotspots.
        if arrival != Arrival::Resend {
            let position = match seen.count {
                1 => &stats.first,
                2 => &stats.second,
                _ => &stats.later,
            };
            position.fetch_add(1, Relaxed);
        }

        let late = match arrival {
            Arrival::Late => Some(&stats.late),
            Arrival::SlowCopy => Some(&stats.slow),
            Arrival::Resend => Some(&stats.resends),
            Arrival::First | Arrival::OnTime => None,
        };
        if let Some(counter) = late {
            counter.fetch_add(1, Relaxed);
            stats.last_late.store(now, Relaxed);
        }

        if let (Some(delay), Arrival::OnTime | Arrival::Late) = (seen.since_first, arrival) {
            stats
                .delay_ms_sum
                .fetch_add(delay.as_millis() as u64, Relaxed);
            stats.delay_count.fetch_add(1, Relaxed);
        }
    }

    /// Every hotspot's current stats, in no particular order.
    pub fn views(&self) -> Vec<HotspotView> {
        let now_minute = now_unix() / 60;
        self.map
            .iter()
            .map(|entry| {
                let s = entry.value();
                let delay_count = s.delay_count.load(Relaxed);
                HotspotView {
                    address: entry.key().clone(),
                    name: s
                        .name
                        .get_or_init(|| deny_lists::animal_name(entry.key()))
                        .clone(),
                    copies: s.copies.load(Relaxed),
                    copies_last_hour: s.last_hour.snapshot_at(now_minute).total(),
                    first: s.first.load(Relaxed),
                    second: s.second.load(Relaxed),
                    later: s.later.load(Relaxed),
                    late: s.late.load(Relaxed),
                    slow: s.slow.load(Relaxed),
                    resends: s.resends.load(Relaxed),
                    denied: s.denied.load(Relaxed),
                    mean_delay_ms: (delay_count > 0)
                        .then(|| s.delay_ms_sum.load(Relaxed) as f64 / delay_count as f64),
                    regions: region_names(s.regions.load(Relaxed)),
                    first_seen: s.first_seen.load(Relaxed),
                    last_seen: s.last_seen.load(Relaxed),
                    last_late: s.last_late.load(Relaxed),
                }
            })
            .collect()
    }

    /// Drop hotspots not seen within [`RETENTION`] of `now`. Returns how many.
    pub fn prune(&self, now: u64) -> usize {
        let cutoff = now.saturating_sub(RETENTION.as_secs());
        let before = self.map.len();
        self.map.retain(|_, s| s.last_seen.load(Relaxed) >= cutoff);
        let removed = before - self.map.len();
        if removed > 0 {
            self.dirty.store(true, Relaxed);
        }
        removed
    }

    /// Prune, then write the registry if anything changed since the last save.
    /// Blocking file I/O: call from a blocking context.
    pub fn save(&self) -> anyhow::Result<()> {
        let pruned = self.prune(now_unix());
        if pruned > 0 {
            tracing::info!(pruned, "dropped hotspots not seen for 30 days");
        }
        if !self.is_persistent() || !self.dirty.swap(false, Relaxed) {
            return Ok(());
        }
        let file = SavedFile {
            version: FORMAT_VERSION,
            saved_at: now_unix(),
            hotspots: self
                .map
                .iter()
                .map(|e| (e.key().clone(), e.value().to_saved()))
                .collect(),
        };
        self.store.save(&file).inspect_err(|_| {
            // Try again next time rather than losing the changes.
            self.dirty.store(true, Relaxed);
        })
    }
}

/// One hotspot as written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedHotspot {
    pub copies: u64,
    pub first: u64,
    pub second: u64,
    pub later: u64,
    pub late: u64,
    pub slow: u64,
    pub resends: u64,
    pub denied: u64,
    pub delay_ms_sum: u64,
    pub delay_count: u64,
    pub regions: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub last_late: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    saved_at: u64,
    hotspots: BTreeMap<String, SavedHotspot>,
}

/// Reads and writes the hotspot registry at a fixed path. No path disables
/// persistence.
#[derive(Debug, Default)]
pub struct HotspotStore {
    path: Option<PathBuf>,
    write_lock: Mutex<()>,
}

impl HotspotStore {
    /// An empty path disables persistence, like `deny_list_store`.
    pub fn new(path: &Path) -> Self {
        Self {
            path: (!path.as_os_str().is_empty()).then(|| path.to_path_buf()),
            write_lock: Mutex::new(()),
        }
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn load(&self) -> anyhow::Result<BTreeMap<String, SavedHotspot>> {
        let Some(path) = &self.path else {
            return Ok(BTreeMap::new());
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        };
        let file: SavedFile = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        if file.version != FORMAT_VERSION {
            anyhow::bail!(
                "{} has format version {}, expected {FORMAT_VERSION}",
                path.display(),
                file.version
            );
        }
        Ok(file.hotspots)
    }

    /// Move a bad file aside so the next save starts clean without destroying it.
    fn quarantine(&self) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        let quarantined = path.with_extension("corrupt");
        std::fs::rename(path, &quarantined).ok()?;
        Some(quarantined)
    }

    /// Write atomically: to a temporary file in the same directory, then
    /// renamed into place, so a crash mid-write keeps the previous file.
    fn save(&self, file: &SavedFile) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        let body = serde_json::to_vec(file)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| anyhow::anyhow!("replacing {}: {e}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(count: u32, ms: Option<u64>, same_hotspot: bool) -> Seen {
        Seen {
            count,
            since_first: ms.map(Duration::from_millis),
            same_hotspot,
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "mb-hotspots-{label}-{}-{unique}",
                std::process::id()
            ))
            .join("hotspots.json")
    }

    fn view(hotspots: &Hotspots, address: &str) -> HotspotView {
        hotspots
            .views()
            .into_iter()
            .find(|v| v.address == address)
            .unwrap()
    }

    #[test]
    fn records_position_delay_lateness_and_regions() {
        let hotspots = Hotspots::in_memory();
        hotspots.record("a", &seen(1, None, false), Arrival::First, 1, false);
        hotspots.record("a", &seen(2, Some(100), false), Arrival::OnTime, 1, false);
        hotspots.record("a", &seen(3, Some(500), false), Arrival::Late, 99, true);
        hotspots.record("a", &seen(4, Some(9_000), true), Arrival::Resend, 1, false);

        let a = view(&hotspots, "a");
        assert_eq!(a.copies, 4);
        assert_eq!((a.first, a.second, a.later), (1, 1, 1));
        assert_eq!((a.late, a.slow, a.resends, a.denied), (1, 0, 1, 1));
        assert_eq!(a.mean_delay_ms, Some(300.0));
        assert_eq!(a.copies_last_hour, 4);
        assert_eq!(
            a.regions,
            vec![deny_lists::region_label(1), "other".to_string()]
        );
        assert!(a.last_late > 0);
        assert!(!a.name.is_empty());
    }

    #[test]
    fn empty_hotspot_is_ignored() {
        let hotspots = Hotspots::in_memory();
        hotspots.record("", &seen(1, None, false), Arrival::First, 0, false);
        assert!(hotspots.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let path = temp_path("roundtrip");
        let hotspots = Hotspots::load(HotspotStore::new(&path));
        hotspots.record("a", &seen(1, None, false), Arrival::First, 1, false);
        hotspots.record("b", &seen(2, Some(40), false), Arrival::OnTime, 2, false);
        hotspots.save().unwrap();

        let reloaded = Hotspots::load(HotspotStore::new(&path));
        assert_eq!(reloaded.len(), 2);
        let (before, after) = (view(&hotspots, "b"), view(&reloaded, "b"));
        assert_eq!(after.copies, before.copies);
        assert_eq!(after.second, 1);
        assert_eq!(after.mean_delay_ms, Some(40.0));
        assert_eq!(after.regions, before.regions);
        assert_eq!(after.first_seen, before.first_seen);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn unchanged_registry_is_not_rewritten() {
        let path = temp_path("dirty");
        let hotspots = Hotspots::load(HotspotStore::new(&path));
        hotspots.save().unwrap();
        assert!(!path.exists(), "nothing recorded, nothing to write");

        hotspots.record("a", &seen(1, None, false), Arrival::First, 1, false);
        hotspots.save().unwrap();
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        hotspots.save().unwrap();
        assert!(!path.exists(), "no changes since the last save");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stale_hotspots_are_pruned() {
        let hotspots = Hotspots::in_memory();
        hotspots.record("a", &seen(1, None, false), Arrival::First, 1, false);
        let now = now_unix();
        assert_eq!(hotspots.prune(now), 0);
        assert_eq!(hotspots.prune(now + RETENTION.as_secs() + 1), 1);
        assert!(hotspots.is_empty());
    }

    #[test]
    fn corrupt_file_is_quarantined_and_registry_starts_empty() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let hotspots = Hotspots::load(HotspotStore::new(&path));
        assert!(hotspots.is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("corrupt").exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
