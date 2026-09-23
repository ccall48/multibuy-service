use crate::api::{self, ApiState};
use std::net::SocketAddr;

pub struct ApiServer {
    state: ApiState,
    listen: SocketAddr,
}

impl ApiServer {
    pub fn new(state: ApiState, listen: SocketAddr) -> Self {
        Self { state, listen }
    }

    async fn run(self, shutdown: triggered::Listener) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.listen).await?;
        tracing::info!("admin API and dashboard listening @ {}", self.listen);

        axum::serve(listener, api::router(self.state))
            .with_graceful_shutdown(shutdown)
            .await?;

        tracing::info!("admin API stopped");
        Ok(())
    }
}

impl task_manager::ManagedTask for ApiServer {
    fn start_task(self: Box<Self>, shutdown: triggered::Listener) -> task_manager::TaskFuture {
        task_manager::spawn(self.run(shutdown))
    }
}
