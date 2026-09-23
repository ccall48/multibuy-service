use crate::connections;
use crate::state::State;
use helium_proto::services::multi_buy::Server as MultiBuyServer;
use std::net::SocketAddr;
use std::time::Duration;

/// How often to ping each client over HTTP/2. Keeps HPR's long-lived
/// connection from looking idle to a NAT, firewall or load balancer in between
/// (common idle timeouts are 60s–350s), and notices a dead peer.
pub const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
/// How long a ping may go unanswered before the connection is closed.
pub const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// OS-level TCP keepalive, for middleboxes that only count TCP traffic.
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

pub struct GrpcServer {
    state: State,
    listen: SocketAddr,
}

impl GrpcServer {
    pub fn new(state: State, listen: SocketAddr) -> Self {
        Self { state, listen }
    }

    async fn run(self, shutdown: triggered::Listener) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.listen).await?;
        tracing::info!("gRPC server listening @ {:?}", self.listen);

        let incoming =
            connections::tracked_incoming(listener, self.state.connections(), Some(TCP_KEEPALIVE));

        server_builder()
            .add_service(MultiBuyServer::new(self.state))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await?;

        tracing::info!("gRPC server stopped");
        Ok(())
    }
}

/// The tonic server with this service's connection settings. Shared with the
/// integration tests so they exercise the same configuration.
pub fn server_builder() -> tonic::transport::Server {
    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(HTTP2_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(HTTP2_KEEPALIVE_TIMEOUT))
}

impl task_manager::ManagedTask for GrpcServer {
    fn start_task(self: Box<Self>, shutdown: triggered::Listener) -> task_manager::TaskFuture {
        task_manager::spawn(self.run(shutdown))
    }
}
