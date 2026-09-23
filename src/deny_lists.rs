use dashmap::DashSet;
use helium_proto::services::multi_buy::MultiBuyIncReqV1;
use helium_proto::Region;

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
        let hotspots: DashSet<String> = hotspot_keys_b58
            .iter()
            .filter(|k| !k.is_empty())
            .cloned()
            .collect();

        let regions = DashSet::new();
        for name in region_names {
            if name.is_empty() {
                continue;
            }
            regions.insert(parse_region(name)? as i32);
        }

        Ok(Self { hotspots, regions })
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
