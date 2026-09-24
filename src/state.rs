use crate::cache::Cache;
use crate::connections::Connections;
use crate::deny_lists::{DenyListStore, DenyLists};
use crate::settings::Settings;
use crate::traffic::Traffic;
use helium_proto::services::multi_buy::{multi_buy_server, MultiBuyIncReqV1, MultiBuyIncResV1};
use std::sync::Arc;
use tonic::Request;

pub struct State {
    cache: Arc<Cache>,
    deny_lists: Arc<DenyLists>,
    store: Arc<DenyListStore>,
    traffic: Arc<Traffic>,
    connections: Arc<Connections>,
}

impl State {
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        let store = DenyListStore::new(&settings.deny_list_store);

        // A bad store file shouldn't keep the service down — HPRs losing
        // multibuy coordination is worse than starting on the configured lists —
        // so report it, move it aside, and carry on.
        let deltas = match store.load() {
            Ok(deltas) => deltas,
            Err(e) => {
                tracing::error!("could not read persisted deny-list changes: {e}");
                match store.quarantine() {
                    Some(path) => tracing::warn!(
                        "moved the unreadable deny-list store to {} and started from settings",
                        path.display()
                    ),
                    None => tracing::warn!("starting deny lists from settings only"),
                }
                Default::default()
            }
        };

        let (deny_lists, suppressed) = DenyLists::from_config_and_deltas_reporting(
            &settings.denied_hotspots,
            &settings.denied_regions,
            &deltas,
        )?;

        for entry in suppressed {
            tracing::warn!(
                "{entry} is in the settings deny list but was removed through the admin API; \
                 leaving it allowed"
            );
        }

        if store.is_enabled() {
            tracing::info!(
                "deny-list changes persist to {}",
                settings.deny_list_store.display()
            );
        } else {
            tracing::warn!("deny_list_store is empty; deny-list changes will be lost on restart");
        }

        crate::metrics::set_deny_list_size("hotspots", deny_lists.hotspots().len());
        crate::metrics::set_deny_list_size("regions", deny_lists.region_names().len());

        Ok(Self {
            cache: Arc::new(Cache::new()),
            deny_lists: Arc::new(deny_lists),
            store: Arc::new(store),
            traffic: Arc::new(Traffic::new(settings.lns_dedup_window)),
            connections: Arc::new(Connections::new()),
        })
    }

    pub fn cache(&self) -> Arc<Cache> {
        self.cache.clone()
    }

    /// The live deny lists. Shared with the admin API so operator changes apply
    /// without a restart.
    pub fn deny_lists(&self) -> Arc<DenyLists> {
        self.deny_lists.clone()
    }

    /// Where deny-list changes are persisted.
    pub fn deny_list_store(&self) -> Arc<DenyListStore> {
        self.store.clone()
    }

    /// Per-second request history. Shared with the admin API so the dashboard
    /// can show when requests stopped arriving.
    pub fn traffic(&self) -> Arc<Traffic> {
        self.traffic.clone()
    }

    /// Open/close history of gRPC client connections, shared with the admin
    /// API so reconnects can be lined up against traffic silences.
    pub fn connections(&self) -> Arc<Connections> {
        self.connections.clone()
    }
}

#[tonic::async_trait]
impl multi_buy_server::MultiBuy for State {
    async fn inc(
        &self,
        request: Request<MultiBuyIncReqV1>,
    ) -> Result<tonic::Response<MultiBuyIncResV1>, tonic::Status> {
        let start = std::time::Instant::now();
        crate::metrics::increment_hit();

        let peer = request.remote_addr().map(|addr| addr.ip());
        let multi_buy_req = request.into_inner();
        // Records a hit against each entry that matched, so the admin API can
        // report which rules are actually doing work.
        let matched = self.deny_lists.check(&multi_buy_req);
        let denied = matched.is_denied();
        let seen = self
            .cache
            .inc(multi_buy_req.key.clone(), &multi_buy_req.hotspot_key);
        let count = seen.count;
        let hotspot = String::from_utf8_lossy(&multi_buy_req.hotspot_key).into_owned();
        self.traffic.record(&seen, &hotspot, peer);
        // The proto region is an integer; log the name so a denial is readable
        // without a lookup table.
        let region = crate::deny_lists::region_label(multi_buy_req.region);

        if denied {
            tracing::info!(
                key = %multi_buy_req.key,
                count,
                hotspot,
                region = %region,
                reason = matched.reason(),
                "denied by deny list"
            );
            crate::metrics::increment_denied();
            crate::metrics::increment_denied_by_reason(matched.reason());
            if matched.region {
                crate::metrics::increment_denied_by_region(region);
            }
        } else {
            tracing::debug!(
                key = %multi_buy_req.key,
                count,
                hotspot,
                region = %region,
                "got inc req"
            );
        }

        crate::metrics::record_request_duration(start.elapsed());

        Ok(tonic::Response::new(MultiBuyIncResV1 { count, denied }))
    }
}
