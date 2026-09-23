pub mod store;

use angry_purple_tiger::AnimalName;
use dashmap::DashSet;
use helium_proto::services::multi_buy::MultiBuyIncReqV1;
use helium_proto::Region;
use std::collections::HashSet;
pub use store::{Delta, DenyListDeltas, DenyListStore};

/// Highest proto enum value scanned when enumerating known regions.
/// `helium.region` is sparse (0..=27 plus `UNKNOWN = 99`), so invalid
/// values in between are simply skipped.
const MAX_REGION_VALUE: i32 = 99;

/// Deny lists ready for O(1) lookups.
///
/// Entries can be added and removed while the gRPC server is serving. Reads on
/// the request path never block writers (and vice versa), so an operator change
/// through the admin API takes effect on the next `inc` request.
#[derive(Default)]
pub struct DenyLists {
    /// Base58check hotspot addresses to deny.
    hotspots: DashSet<String>,
    /// Proto region enum values to deny.
    regions: DashSet<i32>,
    /// The configured lists, kept so runtime changes can be persisted as a
    /// delta against them. See [`store`].
    config_hotspots: HashSet<String>,
    config_regions: HashSet<String>,
}

impl DenyLists {
    /// Parse deny lists from raw config values.
    ///
    /// `hotspot_keys_b58` are base58check-encoded public keys (matching what HPR
    /// now sends as the `hotspot_key` bytes field).
    /// `region_names` are proto enum names like "US915" or "EU868".
    pub fn from_config(
        hotspot_keys_b58: &[String],
        region_names: &[String],
    ) -> anyhow::Result<Self> {
        Self::from_config_and_deltas(hotspot_keys_b58, region_names, &DenyListDeltas::default())
    }

    /// Parse deny lists from config, then apply persisted runtime changes on
    /// top: `(config ∪ added) \ removed`.
    ///
    /// Returns the lists plus any config entries a stored removal suppressed —
    /// surprising enough that the caller should log them.
    pub fn from_config_and_deltas_reporting(
        hotspot_keys_b58: &[String],
        region_names: &[String],
        deltas: &DenyListDeltas,
    ) -> anyhow::Result<(Self, Vec<String>)> {
        let config_hotspots: HashSet<String> = hotspot_keys_b58
            .iter()
            .filter(|k| !k.is_empty())
            .cloned()
            .collect();

        // Normalise configured regions through the proto so the baseline and the
        // stored names agree on spelling.
        let mut config_regions: HashSet<String> = HashSet::new();
        for name in region_names.iter().filter(|n| !n.is_empty()) {
            config_regions.insert(parse_region(name)?.as_str_name().to_string());
        }

        let mut suppressed = Vec::new();

        let hotspots = DashSet::new();
        for key in &config_hotspots {
            if deltas.hotspots.removed.contains(key) {
                suppressed.push(format!("hotspot {key}"));
            } else {
                hotspots.insert(key.clone());
            }
        }
        for key in &deltas.hotspots.added {
            hotspots.insert(key.clone());
        }

        let regions = DashSet::new();
        for name in &config_regions {
            if deltas.regions.removed.contains(name) {
                suppressed.push(format!("region {name}"));
            } else {
                regions.insert(parse_region(name)? as i32);
            }
        }
        for name in &deltas.regions.added {
            regions.insert(parse_region(name)? as i32);
        }

        suppressed.sort();

        Ok((
            Self {
                hotspots,
                regions,
                config_hotspots,
                config_regions,
            },
            suppressed,
        ))
    }

    /// As [`Self::from_config_and_deltas_reporting`], discarding the report.
    pub fn from_config_and_deltas(
        hotspot_keys_b58: &[String],
        region_names: &[String],
        deltas: &DenyListDeltas,
    ) -> anyhow::Result<Self> {
        Self::from_config_and_deltas_reporting(hotspot_keys_b58, region_names, deltas)
            .map(|(lists, _)| lists)
    }

    /// How the live lists currently differ from the configured ones — what gets
    /// written to the store.
    pub fn deltas(&self) -> DenyListDeltas {
        let live_hotspots: HashSet<String> = self.hotspots.iter().map(|k| k.clone()).collect();
        let live_regions: HashSet<String> = self.region_names().into_iter().collect();

        DenyListDeltas {
            version: 1,
            hotspots: Delta::between(&self.config_hotspots, &live_hotspots),
            regions: Delta::between(&self.config_regions, &live_regions),
        }
    }

    /// Returns `true` if the request should be denied based on hotspot key or region.
    pub fn is_denied(&self, req: &MultiBuyIncReqV1) -> bool {
        if let Ok(hotspot_str) = std::str::from_utf8(&req.hotspot_key) {
            if self.hotspots.contains(hotspot_str) {
                return true;
            }
        }
        if self.regions.contains(&req.region) {
            return true;
        }
        false
    }

    /// Currently denied hotspot addresses, sorted.
    pub fn hotspots(&self) -> Vec<String> {
        let mut out: Vec<String> = self.hotspots.iter().map(|k| k.clone()).collect();
        out.sort();
        out
    }

    /// Currently denied region names, sorted.
    ///
    /// A denied value with no matching proto name (only reachable if the proto
    /// enum shrinks under us) is rendered as the raw integer.
    pub fn region_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .regions
            .iter()
            .map(|v| {
                Region::try_from(*v).map_or_else(|_| v.to_string(), |r| r.as_str_name().to_string())
            })
            .collect();
        out.sort();
        out
    }

    /// Add a hotspot address. Returns `true` if it was not already denied.
    pub fn add_hotspot(&self, key_b58: &str) -> bool {
        self.hotspots.insert(key_b58.to_string())
    }

    /// Remove a hotspot address. Returns `true` if it had been denied.
    pub fn remove_hotspot(&self, key_b58: &str) -> bool {
        self.hotspots.remove(key_b58).is_some()
    }

    /// Add a region by proto enum name. Returns `true` if it was not already denied.
    pub fn add_region(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.regions.insert(parse_region(name)? as i32))
    }

    /// Remove a region by proto enum name. Returns `true` if it had been denied.
    pub fn remove_region(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.regions.remove(&(parse_region(name)? as i32)).is_some())
    }
}

/// Resolve a proto region enum name, e.g. "US915".
pub fn parse_region(name: &str) -> anyhow::Result<Region> {
    Region::from_str_name(name).ok_or_else(|| anyhow::anyhow!("unknown region: '{}'", name))
}

/// Every region name the proto knows about, in enum order.
///
/// Used by the admin API so callers can discover valid `denied_regions` values
/// instead of guessing at spellings.
pub fn all_region_names() -> Vec<&'static str> {
    (0..=MAX_REGION_VALUE)
        .filter_map(|v| Region::try_from(v).ok())
        .map(|r| r.as_str_name())
        .collect()
}

/// The Angry Purple Tiger animal name for a hotspot address, e.g.
/// "feisty-glass-dalmatian".
///
/// This is the name operators see in Helium explorers and wallets, so showing it
/// beside the raw b58 address makes a deny list reviewable by eye. Derived by
/// hashing the address string, exactly as the rest of the Helium tooling does.
///
/// Deliberately not called on the request path: it is an md5 per call, and a
/// denied region would pay it on every packet.
pub fn animal_name(key_b58: &str) -> String {
    key_b58
        .parse::<AnimalName>()
        .map(|name| name.to_string())
        .unwrap_or_default()
}

/// Validate a base58check-encoded hotspot address.
///
/// HPR sends the b58 address as raw bytes and we match it as an exact string, so
/// a mistyped entry would silently never match. Rejecting undecodable input at
/// the API boundary turns that silent no-op into an error the operator sees.
pub fn validate_hotspot_key(key_b58: &str) -> anyhow::Result<()> {
    if key_b58.is_empty() {
        anyhow::bail!("hotspot key is empty");
    }
    bs58::decode(key_b58)
        .with_check(None)
        .into_vec()
        .map_err(|e| anyhow::anyhow!("not a base58check-encoded hotspot key: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_region_is_skipped() {
        let deny = DenyLists::from_config(&[], &["".into()]).unwrap();
        assert!(deny.regions.is_empty());
    }

    #[test]
    fn empty_string_hotspot_is_skipped() {
        let deny = DenyLists::from_config(&["".into()], &[]).unwrap();
        assert!(deny.hotspots.is_empty());
    }

    fn req(hotspot_key: &[u8], region: i32) -> MultiBuyIncReqV1 {
        MultiBuyIncReqV1 {
            key: "key".to_string(),
            hotspot_key: hotspot_key.to_vec(),
            region,
        }
    }

    #[test]
    fn added_region_denies_immediately() {
        let deny = DenyLists::default();
        assert!(!deny.is_denied(&req(b"", Region::Eu868 as i32)));

        assert!(deny.add_region("EU868").unwrap());
        assert!(deny.is_denied(&req(b"", Region::Eu868 as i32)));

        // Adding twice is a no-op, and removal takes effect immediately.
        assert!(!deny.add_region("EU868").unwrap());
        assert!(deny.remove_region("EU868").unwrap());
        assert!(!deny.is_denied(&req(b"", Region::Eu868 as i32)));
        assert!(!deny.remove_region("EU868").unwrap());
    }

    #[test]
    fn added_hotspot_denies_immediately() {
        let hotspot = "13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv";
        let deny = DenyLists::default();
        assert!(!deny.is_denied(&req(hotspot.as_bytes(), 0)));

        assert!(deny.add_hotspot(hotspot));
        assert!(deny.is_denied(&req(hotspot.as_bytes(), 0)));
        assert!(!deny.add_hotspot(hotspot));

        assert!(deny.remove_hotspot(hotspot));
        assert!(!deny.is_denied(&req(hotspot.as_bytes(), 0)));
        assert!(!deny.remove_hotspot(hotspot));
    }

    #[test]
    fn unknown_region_name_is_rejected() {
        let deny = DenyLists::default();
        assert!(deny.add_region("NOPE915").is_err());
        assert!(deny.remove_region("NOPE915").is_err());
    }

    #[test]
    fn listings_are_sorted_and_named() {
        let deny = DenyLists::default();
        deny.add_region("US915").unwrap();
        deny.add_region("EU868").unwrap();
        deny.add_hotspot("zzz");
        deny.add_hotspot("aaa");

        assert_eq!(deny.region_names(), vec!["EU868", "US915"]);
        assert_eq!(deny.hotspots(), vec!["aaa", "zzz"]);
    }

    #[test]
    fn all_region_names_covers_proto() {
        let names = all_region_names();
        assert!(names.contains(&"US915"));
        assert!(names.contains(&"EU868"));
        assert!(names.contains(&"UNKNOWN"));
        // Every name round-trips back through the parser.
        for name in names {
            assert!(parse_region(name).is_ok(), "{name} should parse");
        }
    }

    #[test]
    fn animal_name_matches_helium_tooling() {
        // Reference pair from the angry-purple-tiger crate's own test vector.
        assert_eq!(
            animal_name("112CuoXo7WCcp6GGwDNBo6H5nKXGH45UNJ39iEefdv2mwmnwdFt8"),
            "feisty-glass-dalmatian"
        );
        // Stable and of the adjective-color-animal shape for other addresses.
        let name = animal_name("13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv");
        assert_eq!(name.split('-').count(), 3, "name was {name}");
        assert_eq!(
            name,
            animal_name("13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv")
        );
    }

    #[test]
    fn deltas_are_empty_for_a_fresh_config() {
        let deny = DenyLists::from_config(&["hotspot-a".into()], &["EU868".into()]).unwrap();
        let deltas = deny.deltas();
        assert!(deltas.hotspots.is_empty());
        assert!(deltas.regions.is_empty());
    }

    #[test]
    fn deltas_capture_additions_and_removals_against_config() {
        let deny = DenyLists::from_config(&["hotspot-a".into()], &["EU868".into()]).unwrap();

        deny.add_hotspot("hotspot-b");
        deny.remove_hotspot("hotspot-a");
        deny.add_region("KR920").unwrap();
        deny.remove_region("EU868").unwrap();

        let deltas = deny.deltas();
        assert_eq!(deltas.hotspots.added, vec!["hotspot-b"]);
        assert_eq!(deltas.hotspots.removed, vec!["hotspot-a"]);
        assert_eq!(deltas.regions.added, vec!["KR920"]);
        assert_eq!(deltas.regions.removed, vec!["EU868"]);
    }

    #[test]
    fn stored_deltas_are_applied_over_config() {
        let deltas = DenyListDeltas {
            version: 1,
            hotspots: Delta {
                added: vec!["hotspot-b".into()],
                removed: vec!["hotspot-a".into()],
            },
            regions: Delta {
                added: vec!["KR920".into()],
                removed: vec!["EU868".into()],
            },
        };

        let (deny, suppressed) = DenyLists::from_config_and_deltas_reporting(
            &["hotspot-a".into()],
            &["EU868".into()],
            &deltas,
        )
        .unwrap();

        assert_eq!(deny.hotspots(), vec!["hotspot-b"]);
        assert_eq!(deny.region_names(), vec!["KR920"]);
        // The config entries a tombstone suppressed are reported for logging.
        assert_eq!(suppressed, vec!["hotspot hotspot-a", "region EU868"]);

        // Round-tripping reproduces the same deltas.
        assert_eq!(deny.deltas(), deltas);
    }

    #[test]
    fn config_entries_added_after_a_restart_still_apply() {
        // A stored delta must not freeze the configured baseline: a region newly
        // added to the settings file takes effect even though a store exists.
        let deltas = DenyListDeltas {
            version: 1,
            regions: Delta {
                added: vec!["KR920".into()],
                removed: vec![],
            },
            ..Default::default()
        };

        let deny =
            DenyLists::from_config_and_deltas(&[], &["EU868".into(), "AU915".into()], &deltas)
                .unwrap();
        assert_eq!(deny.region_names(), vec!["AU915", "EU868", "KR920"]);
    }

    #[test]
    fn hotspot_key_validation() {
        assert!(
            validate_hotspot_key("13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv").is_ok()
        );
        assert!(validate_hotspot_key("").is_err());
        assert!(validate_hotspot_key("not-a-key").is_err());
        // Valid base58 alphabet but a broken checksum.
        assert!(
            validate_hotspot_key("13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJw").is_err()
        );
    }
}
