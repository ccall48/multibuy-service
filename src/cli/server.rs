use crate::{
    api::ApiState,
    settings::Settings,
    state::State,
    tasks::{api_server::ApiServer, cleanup::CacheCleanup, grpc_server::GrpcServer},
};
use metrics_exporter_prometheus::PrometheusHandle;
use task_manager::TaskManager;

#[derive(Debug, clap::Args)]
pub struct Server {}

impl Server {
    pub async fn run(
        &self,
        settings: &Settings,
        metrics_handle: PrometheusHandle,
    ) -> anyhow::Result<()> {
        tracing::info!("starting server");

        let grpc_state = State::new(settings)?;
        let cache_cleanup = CacheCleanup::new(&grpc_state, settings.cleanup_timeout);
        let grpc_listen = settings.grpc_listen;

        let mut builder = TaskManager::builder();

        if settings.api.enabled {
            if settings.api.auth_token.is_none() {
                tracing::warn!(
                    listen = %settings.api.listen,
                    "admin API is unauthenticated; set api.auth_token (MB__API__AUTH_TOKEN) \
                     or bind it to a trusted interface"
                );
            }
            let api_state = ApiState::new(
                grpc_state.deny_lists(),
                grpc_state.deny_list_store(),
                metrics_handle,
                settings.api.auth_token.clone(),
                grpc_listen,
                settings.metrics.endpoint,
            );
            builder = builder.add_named("api", ApiServer::new(api_state, settings.api.listen));
        } else {
            tracing::info!("admin API disabled");
        }

        builder
            .add_named("grpc", GrpcServer::new(grpc_state, grpc_listen))
            .add_named("cache-cleanup", cache_cleanup)
            .build()
            .start()
            .await
            .map_err(anyhow::Error::from)
    }
}
