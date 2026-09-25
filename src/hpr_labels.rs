//! Operator-assigned names for HPR client addresses, e.g. "Frankfurt".
//!
//! HPRs are only known to this service by IP. The dashboard can look up a rough
//! location, but an operator usually knows better — and a name like "EU-1" may
//! be more useful than a city anyway. Labels are few and change rarely, so the
//! whole map is written to disk on every change.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Longest label accepted, in characters.
pub const MAX_LABEL_CHARS: usize = 64;

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    labels: BTreeMap<String, String>,
}

/// The canonical text form of an IP, so "::ffff:1.2.3.4" and "1.2.3.4" (and
/// any spelling of an IPv6 address) share one label.
pub fn canonical_ip(ip: &str) -> anyhow::Result<String> {
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("'{ip}' is not an IP address"))?;
    Ok(ip.to_canonical().to_string())
}

/// Check and normalise an `(ip, label)` pair before it is stored: the
/// canonical IP and the trimmed label.
pub fn validate(ip: &str, label: &str) -> anyhow::Result<(String, String)> {
    Ok((canonical_ip(ip)?, clean_label(label)?))
}

fn clean_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    if label.is_empty() {
        anyhow::bail!("label is empty");
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        anyhow::bail!("label is longer than {MAX_LABEL_CHARS} characters");
    }
    if label.chars().any(char::is_control) {
        anyhow::bail!("label contains control characters");
    }
    Ok(label.to_string())
}

#[derive(Debug, Default)]
pub struct HprLabels {
    labels: RwLock<BTreeMap<String, String>>,
    path: Option<PathBuf>,
}

impl HprLabels {
    /// Load labels from `path`; an empty path keeps them in memory only. A
    /// missing file starts empty; a bad one is moved aside and reported.
    pub fn load(path: &Path) -> Self {
        let path = (!path.as_os_str().is_empty()).then(|| path.to_path_buf());
        let labels = match &path {
            None => {
                tracing::warn!("hpr_label_store is empty; HPR labels will be lost on restart");
                BTreeMap::new()
            }
            Some(path) => match read(path) {
                Ok(labels) => labels,
                Err(e) => {
                    tracing::error!("could not read HPR labels: {e}");
                    let quarantined = path.with_extension("corrupt");
                    if std::fs::rename(path, &quarantined).is_ok() {
                        tracing::warn!(
                            "moved the unreadable HPR label file to {} and started empty",
                            quarantined.display()
                        );
                    }
                    BTreeMap::new()
                }
            },
        };
        Self {
            labels: RwLock::new(labels),
            path,
        }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn is_persistent(&self) -> bool {
        self.path.is_some()
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.read_lock().clone()
    }

    /// Set the label for `ip`. Input errors come from [`validate`]; call it
    /// first to tell them apart from a failed save, after which the change
    /// still applies in memory.
    pub fn set(&self, ip: &str, label: &str) -> anyhow::Result<()> {
        let (ip, label) = validate(ip, label)?;
        let snapshot = {
            let mut labels = self.write_lock();
            labels.insert(ip, label);
            labels.clone()
        };
        self.save(&snapshot)
    }

    /// Remove the label for `ip`. Returns whether there was one.
    pub fn remove(&self, ip: &str) -> anyhow::Result<bool> {
        let ip = canonical_ip(ip)?;
        let (removed, snapshot) = {
            let mut labels = self.write_lock();
            let removed = labels.remove(&ip).is_some();
            (removed, labels.clone())
        };
        if removed {
            self.save(&snapshot)?;
        }
        Ok(removed)
    }

    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, String>> {
        self.labels.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, String>> {
        self.labels.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Write atomically (temp file, then rename) so a crash keeps the old file.
    fn save(&self, labels: &BTreeMap<String, String>) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(&SavedFile {
            version: FORMAT_VERSION,
            labels: labels.clone(),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| anyhow::anyhow!("replacing {}: {e}", path.display()))
    }
}

fn read(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
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
    Ok(file.labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("mb-hpr-{label}-{}-{unique}", std::process::id()))
            .join("hpr-labels.json")
    }

    #[test]
    fn set_replace_and_remove() {
        let labels = HprLabels::in_memory();
        labels.set("3.72.47.84", "  Frankfurt ").unwrap();
        assert_eq!(labels.all()["3.72.47.84"], "Frankfurt");
        labels.set("3.72.47.84", "Portland").unwrap();
        assert_eq!(labels.all()["3.72.47.84"], "Portland");
        assert!(labels.remove("3.72.47.84").unwrap());
        assert!(!labels.remove("3.72.47.84").unwrap());
        assert!(labels.all().is_empty());
    }

    #[test]
    fn ipv4_mapped_addresses_share_a_label() {
        let labels = HprLabels::in_memory();
        labels.set("::ffff:18.236.140.3", "Portland").unwrap();
        assert_eq!(labels.all()["18.236.140.3"], "Portland");
    }

    #[test]
    fn rejects_bad_input() {
        let labels = HprLabels::in_memory();
        assert!(labels.set("not-an-ip", "x").is_err());
        assert!(labels.set("1.2.3.4", "   ").is_err());
        assert!(labels
            .set("1.2.3.4", &"x".repeat(MAX_LABEL_CHARS + 1))
            .is_err());
        assert!(labels.set("1.2.3.4", "bad\nlabel").is_err());
        assert!(labels.all().is_empty());
    }

    #[test]
    fn labels_survive_a_reload() {
        let path = temp_path("reload");
        let labels = HprLabels::load(&path);
        labels.set("44.245.6.101", "Singapore").unwrap();
        let reloaded = HprLabels::load(&path);
        assert_eq!(reloaded.all()["44.245.6.101"], "Singapore");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "nope").unwrap();
        let labels = HprLabels::load(&path);
        assert!(labels.all().is_empty());
        assert!(path.with_extension("corrupt").exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
