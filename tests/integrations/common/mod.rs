use helium_proto::services::multi_buy::{
    multi_buy_client::MultiBuyClient, multi_buy_server::MultiBuyServer, MultiBuyIncReqV1,
    MultiBuyIncResV1,
};
use multi_buy_service::settings::Settings;
use multi_buy_service::state::State;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tonic::transport::Channel;

/// Find an available port by binding to port 0.
pub async fn available_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// Build a test Settings with defaults.
///
/// Persistence is off unless a test asks for it, so tests don't write
/// deny-list or hotspot files into the working directory.
pub fn test_settings() -> Settings {
    test_settings_with_cleanup(Duration::from_secs(60 * 30))
}

/// A unique temp path for a test's deny-list store.
pub fn temp_store_path(label: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mb-test-{label}-{}-{unique}", std::process::id()))
}

/// Build a test Settings with deny lists configured.
pub fn test_settings_with_deny_lists(
    denied_hotspots: Vec<String>,
    denied_regions: Vec<String>,
) -> Settings {
    let mut s = test_settings();
    s.denied_hotspots = denied_hotspots;
    s.denied_regions = denied_regions;
    s
}

/// Build a test Settings with custom cleanup timeout.
pub fn test_settings_with_cleanup(cleanup_timeout: Duration) -> Settings {
    Settings::new::<String>(None).map_or_else(
        |_| panic!("failed to create default settings"),
        |mut s| {
            s.grpc_listen = "127.0.0.1:0".parse().unwrap();
            s.cleanup_timeout = cleanup_timeout;
            s.deny_list_store = std::path::PathBuf::new();
            s.hotspot_store = std::path::PathBuf::new();
            s
        },
    )
}

/// Start the gRPC server on the given address and return a shutdown trigger.
/// The server runs in a background task.
pub async fn start_server(settings: &Settings, addr: SocketAddr) -> triggered::Trigger {
    let state = State::new(settings).unwrap();
    let (trigger, shutdown) = triggered::trigger();

    let incoming = TcpListener::bind(addr).await.unwrap();
    let incoming_stream = tokio_stream::wrappers::TcpListenerStream::new(incoming);

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MultiBuyServer::new(state))
            .serve_with_incoming_shutdown(incoming_stream, shutdown)
            .await
            .unwrap();
    });

    trigger
}

/// Start the gRPC server and also run the cache cleanup task.
/// Returns (shutdown_trigger, cache_arc) for inspection.
pub async fn start_server_with_cleanup(
    settings: &Settings,
    addr: SocketAddr,
) -> triggered::Trigger {
    let state = State::new(settings).unwrap();
    let cache = state.cache();
    let cleanup_timeout = settings.cleanup_timeout;
    let (trigger, shutdown) = triggered::trigger();

    let incoming = TcpListener::bind(addr).await.unwrap();
    let incoming_stream = tokio_stream::wrappers::TcpListenerStream::new(incoming);

    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MultiBuyServer::new(state))
            .serve_with_incoming_shutdown(incoming_stream, shutdown_clone)
            .await
            .unwrap();
    });

    // Spawn cleanup task
    let shutdown_clone = shutdown;
    tokio::spawn(async move {
        use multi_buy_service::tasks::cleanup::CacheCleanup;
        let cleanup = CacheCleanup::from_cache(cache, cleanup_timeout);
        cleanup.run_until(shutdown_clone).await.unwrap();
    });

    trigger
}

/// Start the gRPC server plus the admin API, sharing one `State` (and therefore
/// one set of deny lists) between them — the same wiring the server binary uses.
///
/// Returns (shutdown_trigger, api_addr).
pub async fn start_server_with_api(
    settings: &Settings,
    grpc_addr: SocketAddr,
) -> (triggered::Trigger, SocketAddr) {
    let state = State::new(settings).unwrap();
    let api_state = multi_buy_service::api::ApiState::new(
        &state,
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        settings.api.auth_token.clone(),
        grpc_addr,
        settings.metrics.endpoint,
    );
    let (trigger, shutdown) = triggered::trigger();

    // Same listener wiring as the server binary, so connections are tracked.
    let grpc_stream = multi_buy_service::connections::tracked_incoming(
        TcpListener::bind(grpc_addr).await.unwrap(),
        state.connections(),
        None,
    );

    let api_incoming = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_incoming.local_addr().unwrap();

    let grpc_shutdown = shutdown.clone();
    tokio::spawn(async move {
        multi_buy_service::tasks::grpc_server::server_builder()
            .add_service(MultiBuyServer::new(state))
            .serve_with_incoming_shutdown(grpc_stream, grpc_shutdown)
            .await
            .unwrap();
    });

    tokio::spawn(async move {
        axum::serve(api_incoming, multi_buy_service::api::router(api_state))
            .with_graceful_shutdown(shutdown)
            .await
            .unwrap();
    });

    (trigger, api_addr)
}

/// A minimal HTTP request against the admin API, returning (status, body).
///
/// Hand-rolled to keep an HTTP client out of the dependency tree for one test
/// helper; the API only needs simple, single-shot HTTP/1.1 requests here.
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    match body {
        Some(body) => {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            request.push_str(body);
        }
        None => request.push_str("\r\n"),
    }

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let response = String::from_utf8_lossy(&raw).into_owned();

    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in response: {response}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();

    // Responses here are small and sent in one chunk, so strip the chunked
    // framing rather than implementing a full decoder.
    let body = if response
        .to_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body.split("\r\n")
            .filter(|line| !line.is_empty() && u64::from_str_radix(line.trim(), 16).is_err())
            .collect::<Vec<_>>()
            .join("")
    } else {
        body
    };

    (status, body)
}

/// Connect a MultiBuyClient to the given address.
pub async fn connect_client(addr: SocketAddr) -> MultiBuyClient<Channel> {
    let url = format!("http://{addr}");
    MultiBuyClient::connect(url).await.unwrap()
}

/// Send an inc request with the given key, hotspot_key, and region.
pub async fn inc(
    client: &mut MultiBuyClient<Channel>,
    key: &str,
    hotspot_key: Vec<u8>,
    region: i32,
) -> MultiBuyIncResV1 {
    let req = MultiBuyIncReqV1 {
        key: key.to_string(),
        hotspot_key,
        region,
    };
    client.inc(req).await.unwrap().into_inner()
}
