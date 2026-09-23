//! On-disk persistence for deny-list changes made through the admin API.
//!
//! The settings file stays the baseline: the store only records how the live
//! lists differ from it, as an `added`/`removed` pair per list. The effective
//! deny list at startup is therefore `(config ∪ added) \ removed`.
//!
//! Storing deltas rather than a snapshot keeps the settings file meaningful
//! after the first API call — a new entry added to `denied_regions` in a
//! deployment manifest still takes effect on the next restart, while an entry an
//! operator removed at 3am stays removed.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Current on-disk format version. Bump when the shape changes incompatibly.
const FORMAT_VERSION: u32 = 1;

/// How the live deny lists differ from the configured ones.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DenyListDeltas {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub hotspots: Delta,
    #[serde(default)]
    pub regions: Delta,
}

/// Entries added to, and removed from, one configured list.
///
/// `removed` is a tombstone list: without it, un-denying something that came
/// from the settings file would come back on the next restart.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Delta {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
}

impl Delta {
    /// Derive a delta by comparing the live set against the configured one.
    ///
    /// Deriving on save (rather than tracking each edit) means re-adding
    /// something that was removed simply drops its tombstone, with no
    /// bookkeeping to get wrong.
    pub fn between(configured: &HashSet<String>, live: &HashSet<String>) -> Self {
        let mut added: Vec<String> = live.difference(configured).cloned().collect();
        let mut removed: Vec<String> = configured.difference(live).cloned().collect();
        added.sort();
        removed.sort();
        Self { added, removed }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

/// Reads and writes [`DenyListDeltas`] at a fixed path.
///
/// A store with no path is a no-op, which is how persistence is disabled.
#[derive(Debug, Default)]
pub struct DenyListStore {
    path: Option<PathBuf>,
    /// Serialises writers so concurrent API calls can't collide on the
    /// temporary file. Writes are small and synchronous.
    write_lock: Mutex<()>,
}

impl DenyListStore {
    /// An empty path disables persistence, matching how the deny-list settings
    /// already treat empty environment variables.
    pub fn new(path: &Path) -> Self {
        let path = if path.as_os_str().is_empty() {
            None
        } else {
            Some(path.to_path_buf())
        };
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Load persisted deltas.
    ///
    /// A missing file means "nothing persisted yet". An unreadable or malformed
    /// file is moved aside to `<path>.corrupt` and reported, rather than either
    /// aborting startup (which would take multibuy coordination offline over a
    /// bad ops file) or quietly overwriting evidence.
    pub fn load(&self) -> anyhow::Result<DenyListDeltas> {
        let Some(path) = &self.path else {
            return Ok(DenyListDeltas::default());
        };

        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DenyListDeltas::default())
            }
            Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        };

        match serde_json::from_str::<DenyListDeltas>(&raw) {
            Ok(deltas) if deltas.version == FORMAT_VERSION => Ok(deltas),
            Ok(deltas) => Err(anyhow::anyhow!(
                "{} has format version {}, expected {FORMAT_VERSION}",
                path.display(),
                deltas.version
            )),
            Err(e) => Err(anyhow::anyhow!("parsing {}: {e}", path.display())),
        }
    }

    /// Move a bad store file aside so the next save starts clean without
    /// destroying what was there.
    pub fn quarantine(&self) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        let quarantined = path.with_extension("corrupt");
        std::fs::rename(path, &quarantined).ok()?;
        Some(quarantined)
    }

    /// Write deltas out, atomically.
    ///
    /// Writes go to a temporary file in the same directory and are renamed into
    /// place, so a crash mid-write leaves the previous file intact rather than a
    /// truncated one.
    pub fn save(&self, deltas: &DenyListDeltas) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };

        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
            }
        }

        let body = serde_json::to_string_pretty(deltas)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| anyhow::anyhow!("replacing {}: {e}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn delta_between_sets() {
        let delta = Delta::between(&set(&["a", "b"]), &set(&["b", "c"]));
        assert_eq!(delta.added, vec!["c"]);
        assert_eq!(delta.removed, vec!["a"]);
    }

    #[test]
    fn delta_is_empty_when_live_matches_config() {
        let delta = Delta::between(&set(&["a"]), &set(&["a"]));
        assert!(delta.is_empty());
    }

    #[test]
    fn readding_a_removed_entry_drops_its_tombstone() {
        // Config has "a"; operator removes it, then puts it back.
        let configured = set(&["a"]);
        assert_eq!(Delta::between(&configured, &set(&[])).removed, vec!["a"]);
        assert!(Delta::between(&configured, &set(&["a"])).is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("mb-store-{}", std::process::id()));
        let path = dir.join("deny-list.json");
        let store = DenyListStore::new(&path);

        let deltas = DenyListDeltas {
            version: FORMAT_VERSION,
            hotspots: Delta {
                added: vec!["hotspot-a".into()],
                removed: vec![],
            },
            regions: Delta {
                added: vec!["EU868".into()],
                removed: vec!["US915".into()],
            },
        };

        store.save(&deltas).unwrap();
        assert_eq!(store.load().unwrap(), deltas);

        // Parent directories are created as needed, and the temp file is gone.
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let store = DenyListStore::new(Path::new("/nonexistent/mb-does-not-exist.json"));
        assert_eq!(store.load().unwrap(), DenyListDeltas::default());
    }

    #[test]
    fn empty_path_disables_the_store() {
        let store = DenyListStore::new(Path::new(""));
        assert!(!store.is_enabled());
        assert!(store.save(&DenyListDeltas::default()).is_ok());
        assert_eq!(store.load().unwrap(), DenyListDeltas::default());
    }

    #[test]
    fn corrupt_file_errors_then_quarantines() {
        let dir = std::env::temp_dir().join(format!("mb-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deny-list.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let store = DenyListStore::new(&path);
        assert!(store.load().is_err());

        let quarantined = store.quarantine().expect("file moved aside");
        assert!(quarantined.exists(), "bad file should be preserved");
        assert!(!path.exists(), "bad file should no longer be in the way");
        assert_eq!(store.load().unwrap(), DenyListDeltas::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn future_format_version_is_rejected() {
        let dir = std::env::temp_dir().join(format!("mb-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deny-list.json");
        std::fs::write(&path, r#"{"version":999,"hotspots":{},"regions":{}}"#).unwrap();

        let store = DenyListStore::new(&path);
        let err = store.load().unwrap_err().to_string();
        assert!(err.contains("999"), "error was {err}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
