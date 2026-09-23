use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
pub use settings::Settings;
use std::net::SocketAddr;

pub mod settings;

const HIT_TOTAL: &str = "multi_buy_hit_total";
const DENIED_TOTAL: &str = "multi_buy_denied_total";
const CACHE_SIZE: &str = "multi_buy_cache_size";
const REQUEST_DURATION: &str = "multi_buy_request_duration_ms";
const COPY_DELAY: &str = "multi_buy_copy_delay_ms";
const DENY_LIST_SIZE: &str = "multi_buy_deny_list_size";
const DENIED_BY_REASON: &str = "multi_buy_denied_by_reason_total";
const DENIED_BY_REGION: &str = "multi_buy_denied_by_region_total";

/// Install the recorder, start the Prometheus scrape endpoint, and return a
/// handle that can render the same payload in-process (used by the dashboard).
pub fn start_metrics(settings: &Settings) -> anyhow::Result<PrometheusHandle> {
    install(settings.endpoint)
}

fn install(socket_addr: SocketAddr) -> anyhow::Result<PrometheusHandle> {
    // `install()` would hide the recorder from us, so build the pieces by hand:
    // same scrape listener, plus a handle we can render from for the dashboard.
    let (recorder, exporter) = PrometheusBuilder::new()
        .with_http_listener(socket_addr)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build Prometheus exporter: {e}"))?;

    let handle = recorder.handle();

    metrics::set_global_recorder(recorder)
        .map_err(|e| anyhow::anyhow!("failed to install metrics recorder: {e}"))?;

    tokio::spawn(async move {
        if let Err(e) = exporter.await {
            tracing::error!("Prometheus scrape endpoint stopped: {e:?}");
        }
    });

    tracing::info!("Metrics scrape endpoint listening on {socket_addr}");

    Ok(handle)
}

pub fn increment_hit() {
    metrics::counter!(HIT_TOTAL).increment(1);
}

pub fn increment_denied() {
    metrics::counter!(DENIED_TOTAL).increment(1);
}

/// Break denials down by which rule matched: "hotspot", "region" or "both".
/// Three series at most, so this is safe to label.
pub fn increment_denied_by_reason(reason: &'static str) {
    metrics::counter!(DENIED_BY_REASON, "reason" => reason).increment(1);
}

/// Denials attributed to a specific region.
///
/// Bounded by the proto enum (~28 values), so the cardinality is safe. There is
/// deliberately no per-hotspot equivalent: that would create one series per
/// denied address, which is unbounded from Prometheus's point of view. Per-hotspot
/// counts are exposed through the admin API instead.
pub fn increment_denied_by_region(region: String) {
    metrics::counter!(DENIED_BY_REGION, "region" => region).increment(1);
}

pub fn set_cache_size(size: f64) {
    metrics::gauge!(CACHE_SIZE).set(size);
}

pub fn inc_cache_size() {
    metrics::gauge!(CACHE_SIZE).increment(1);
}

pub fn record_request_duration(duration: std::time::Duration) {
    metrics::histogram!(REQUEST_DURATION).record(duration.as_secs_f64() * 1000.0);
}

/// How long after the first copy of a packet a later copy arrived. Only
/// recorded for copies within the repeat threshold, so device resends minutes
/// later don't swamp the distribution.
pub fn record_copy_delay(delay: std::time::Duration) {
    metrics::histogram!(COPY_DELAY).record(delay.as_secs_f64() * 1000.0);
}

/// Track the size of a deny list so changes made through the admin API show up
/// on the dashboard and in Prometheus.
pub fn set_deny_list_size(kind: &'static str, size: usize) {
    metrics::gauge!(DENY_LIST_SIZE, "kind" => kind).set(size as f64);
}
