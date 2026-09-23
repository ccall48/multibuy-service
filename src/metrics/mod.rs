use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
pub use settings::Settings;
use std::net::SocketAddr;

pub mod settings;

const HIT_TOTAL: &str = "multi_buy_hit_total";
const DENIED_TOTAL: &str = "multi_buy_denied_total";
const CACHE_SIZE: &str = "multi_buy_cache_size";
const REQUEST_DURATION: &str = "multi_buy_request_duration_ms";
const DENY_LIST_SIZE: &str = "multi_buy_deny_list_size";

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

pub fn set_cache_size(size: f64) {
    metrics::gauge!(CACHE_SIZE).set(size);
}

pub fn inc_cache_size() {
    metrics::gauge!(CACHE_SIZE).increment(1);
}

pub fn record_request_duration(duration: std::time::Duration) {
    metrics::histogram!(REQUEST_DURATION).record(duration.as_millis() as f64);
}

/// Track the size of a deny list so changes made through the admin API show up
/// on the dashboard and in Prometheus.
pub fn set_deny_list_size(kind: &'static str, size: usize) {
    metrics::gauge!(DENY_LIST_SIZE, "kind" => kind).set(size as f64);
}
