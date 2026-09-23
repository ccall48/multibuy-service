//! Admin HTTP API and metrics dashboard.
//!
//! The deny lists are held behind concurrent sets shared with the gRPC handler,
//! so every mutation here applies to the very next `inc` request — no restart,
//! no config reload. Changes are in-memory only: on restart the service falls
//! back to `denied_hotspots` / `denied_regions` from settings.

pub mod settings;

use crate::deny_lists::{self, DenyLists};
use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
pub use settings::Settings;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Everything the handlers need. Cheap to clone (all shared handles).
#[derive(Clone)]
pub struct ApiState {
    deny_lists: Arc<DenyLists>,
    metrics: PrometheusHandle,
    auth_token: Option<Arc<String>>,
    grpc_listen: SocketAddr,
    metrics_endpoint: SocketAddr,
    started_at: Instant,
}

impl ApiState {
    pub fn new(
        deny_lists: Arc<DenyLists>,
        metrics: PrometheusHandle,
        auth_token: Option<String>,
        grpc_listen: SocketAddr,
        metrics_endpoint: SocketAddr,
    ) -> Self {
        Self {
            deny_lists,
            metrics,
            auth_token: auth_token.map(Arc::new),
            grpc_listen,
            metrics_endpoint,
            started_at: Instant::now(),
        }
    }
}

/// Build the admin router.
pub fn router(state: ApiState) -> Router {
    // `/` and `/health` stay open: the dashboard shell needs to load before it
    // can prompt for a token, and health checks shouldn't need credentials.
    let public = Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health));

    let api = Router::new()
        .route("/api/v1/info", get(info))
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/regions", get(known_regions))
        .route("/api/v1/deny-list", get(get_deny_list))
        .route(
            "/api/v1/deny-list/hotspots",
            get(get_hotspots)
                .post(add_hotspots)
                .delete(remove_hotspots_body),
        )
        .route(
            "/api/v1/deny-list/hotspots/{hotspot}",
            delete(remove_hotspot),
        )
        .route(
            "/api/v1/deny-list/regions",
            get(get_regions)
                .post(add_regions)
                .delete(remove_regions_body),
        )
        .route("/api/v1/deny-list/regions/{region}", delete(remove_region))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    public.merge(api).with_state(state)
}

// ---------------------------------------------------------------- errors

pub struct ApiError {
    status: StatusCode,
    message: String,
    details: Vec<String>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            details: Vec::new(),
        }
    }

    fn bad_request(message: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            details,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    details: Vec<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
                details: self.details,
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------- auth

async fn require_auth(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(expected) = state.auth_token.as_deref() else {
        return Ok(next.run(request).await);
    };

    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).trim());

    match presented {
        Some(token) if secret_eq(token, expected) => Ok(next.run(request).await),
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token",
        )),
    }
}

/// Length-independent-ish comparison so a wrong token doesn't leak its prefix.
fn secret_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ---------------------------------------------------------------- read handlers

async fn health() -> &'static str {
    "ok"
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

#[derive(Serialize)]
struct Info {
    version: &'static str,
    grpc_listen: String,
    metrics_endpoint: String,
    uptime_seconds: u64,
    auth_required: bool,
}

async fn info(State(state): State<ApiState>) -> Json<Info> {
    Json(Info {
        version: env!("CARGO_PKG_VERSION"),
        grpc_listen: state.grpc_listen.to_string(),
        metrics_endpoint: state.metrics_endpoint.to_string(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        auth_required: state.auth_token.is_some(),
    })
}

/// The same payload the Prometheus scrape endpoint serves, rendered in-process
/// so the dashboard doesn't need network access to the scrape port.
async fn metrics(State(state): State<ApiState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

#[derive(Serialize)]
struct KnownRegions {
    regions: Vec<&'static str>,
}

async fn known_regions() -> Json<KnownRegions> {
    Json(KnownRegions {
        regions: deny_lists::all_region_names(),
    })
}

#[derive(Serialize)]
struct DenyListView {
    hotspots: Vec<String>,
    regions: Vec<String>,
}

async fn get_deny_list(State(state): State<ApiState>) -> Json<DenyListView> {
    Json(DenyListView {
        hotspots: state.deny_lists.hotspots(),
        regions: state.deny_lists.region_names(),
    })
}

#[derive(Serialize)]
struct HotspotsView {
    count: usize,
    hotspots: Vec<String>,
}

async fn get_hotspots(State(state): State<ApiState>) -> Json<HotspotsView> {
    let hotspots = state.deny_lists.hotspots();
    Json(HotspotsView {
        count: hotspots.len(),
        hotspots,
    })
}

#[derive(Serialize)]
struct RegionsView {
    count: usize,
    regions: Vec<String>,
}

async fn get_regions(State(state): State<ApiState>) -> Json<RegionsView> {
    let regions = state.deny_lists.region_names();
    Json(RegionsView {
        count: regions.len(),
        regions,
    })
}

// ---------------------------------------------------------------- write handlers

/// Accepts either `{"hotspots": ["a", "b"]}` or `{"hotspot": "a"}`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HotspotsBody {
    #[serde(default)]
    hotspots: Vec<String>,
    #[serde(default)]
    hotspot: Option<String>,
}

impl HotspotsBody {
    fn values(self) -> Vec<String> {
        let mut out = self.hotspots;
        out.extend(self.hotspot);
        out
    }
}

/// Accepts either `{"regions": ["US915"]}` or `{"region": "US915"}`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RegionsBody {
    #[serde(default)]
    regions: Vec<String>,
    #[serde(default)]
    region: Option<String>,
}

impl RegionsBody {
    fn values(self) -> Vec<String> {
        let mut out = self.regions;
        out.extend(self.region);
        out
    }
}

#[derive(Serialize)]
struct HotspotMutation {
    /// Entries whose presence in the deny list actually changed.
    changed: Vec<String>,
    /// Entries that were already in (or already absent from) the deny list.
    unchanged: Vec<String>,
    /// The full deny list after the change.
    hotspots: Vec<String>,
}

#[derive(Serialize)]
struct RegionMutation {
    changed: Vec<String>,
    unchanged: Vec<String>,
    regions: Vec<String>,
}

async fn add_hotspots(
    State(state): State<ApiState>,
    Json(body): Json<HotspotsBody>,
) -> Result<Json<HotspotMutation>, ApiError> {
    let values = non_empty(body.values(), "hotspots")?;

    // Validate everything before mutating, so a bad entry can't leave the list
    // half-updated.
    let invalid: Vec<String> = values
        .iter()
        .filter_map(|k| {
            deny_lists::validate_hotspot_key(k)
                .err()
                .map(|e| format!("'{k}': {e}"))
        })
        .collect();
    if !invalid.is_empty() {
        return Err(ApiError::bad_request(
            "invalid hotspot key(s); nothing was changed",
            invalid,
        ));
    }

    let (changed, unchanged) = partition(&values, |k| state.deny_lists.add_hotspot(k));
    log_change("added", "hotspots", &changed);
    Ok(Json(hotspot_mutation(&state, changed, unchanged)))
}

async fn remove_hotspots_body(
    State(state): State<ApiState>,
    Json(body): Json<HotspotsBody>,
) -> Result<Json<HotspotMutation>, ApiError> {
    let values = non_empty(body.values(), "hotspots")?;
    let (changed, unchanged) = partition(&values, |k| state.deny_lists.remove_hotspot(k));
    log_change("removed", "hotspots", &changed);
    Ok(Json(hotspot_mutation(&state, changed, unchanged)))
}

async fn remove_hotspot(
    State(state): State<ApiState>,
    Path(hotspot): Path<String>,
) -> Result<Json<HotspotMutation>, ApiError> {
    let (changed, unchanged) = partition(&[hotspot], |k| state.deny_lists.remove_hotspot(k));
    log_change("removed", "hotspots", &changed);
    Ok(Json(hotspot_mutation(&state, changed, unchanged)))
}

async fn add_regions(
    State(state): State<ApiState>,
    Json(body): Json<RegionsBody>,
) -> Result<Json<RegionMutation>, ApiError> {
    let values = validated_regions(non_empty(body.values(), "regions")?)?;
    let (changed, unchanged) = try_partition(&values, |r| state.deny_lists.add_region(r))?;
    log_change("added", "regions", &changed);
    Ok(Json(region_mutation(&state, changed, unchanged)))
}

async fn remove_regions_body(
    State(state): State<ApiState>,
    Json(body): Json<RegionsBody>,
) -> Result<Json<RegionMutation>, ApiError> {
    let values = validated_regions(non_empty(body.values(), "regions")?)?;
    let (changed, unchanged) = try_partition(&values, |r| state.deny_lists.remove_region(r))?;
    log_change("removed", "regions", &changed);
    Ok(Json(region_mutation(&state, changed, unchanged)))
}

async fn remove_region(
    State(state): State<ApiState>,
    Path(region): Path<String>,
) -> Result<Json<RegionMutation>, ApiError> {
    let values = validated_regions(vec![region])?;
    let (changed, unchanged) = try_partition(&values, |r| state.deny_lists.remove_region(r))?;
    log_change("removed", "regions", &changed);
    Ok(Json(region_mutation(&state, changed, unchanged)))
}

// ---------------------------------------------------------------- helpers

fn non_empty(values: Vec<String>, field: &str) -> Result<Vec<String>, ApiError> {
    if values.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("no {field} given"),
        ));
    }
    Ok(values)
}

/// Reject unknown region names up front so a batch is all-or-nothing.
fn validated_regions(values: Vec<String>) -> Result<Vec<String>, ApiError> {
    let invalid: Vec<String> = values
        .iter()
        .filter_map(|r| deny_lists::parse_region(r).err().map(|e| e.to_string()))
        .collect();
    if !invalid.is_empty() {
        return Err(ApiError::bad_request(
            "invalid region name(s); nothing was changed",
            invalid,
        ));
    }
    Ok(values)
}

fn partition(values: &[String], mut apply: impl FnMut(&str) -> bool) -> (Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for value in values {
        if apply(value) {
            changed.push(value.clone());
        } else {
            unchanged.push(value.clone());
        }
    }
    (changed, unchanged)
}

fn try_partition(
    values: &[String],
    mut apply: impl FnMut(&str) -> anyhow::Result<bool>,
) -> Result<(Vec<String>, Vec<String>), ApiError> {
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for value in values {
        // Names were validated above, so an error here is a genuine surprise.
        let applied =
            apply(value).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        if applied {
            changed.push(value.clone());
        } else {
            unchanged.push(value.clone());
        }
    }
    Ok((changed, unchanged))
}

fn hotspot_mutation(
    state: &ApiState,
    changed: Vec<String>,
    unchanged: Vec<String>,
) -> HotspotMutation {
    let hotspots = state.deny_lists.hotspots();
    crate::metrics::set_deny_list_size("hotspots", hotspots.len());
    HotspotMutation {
        changed,
        unchanged,
        hotspots,
    }
}

fn region_mutation(
    state: &ApiState,
    changed: Vec<String>,
    unchanged: Vec<String>,
) -> RegionMutation {
    let regions = state.deny_lists.region_names();
    crate::metrics::set_deny_list_size("regions", regions.len());
    RegionMutation {
        changed,
        unchanged,
        regions,
    }
}

fn log_change(action: &str, kind: &str, changed: &[String]) {
    if !changed.is_empty() {
        tracing::info!(
            entries = ?changed,
            "deny list {kind} {action} via admin API"
        );
    }
}
