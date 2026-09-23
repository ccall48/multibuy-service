pub mod server;

use crate::settings::Settings;
use anyhow::Result;
use clap::Parser;
use server::Server;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    /// Optional configuration file to use. If present, the toml file at the
    /// given path will be loaded. Environment variables can override the
    /// settings in the given file.
    #[clap(short = 'c')]
    config: Option<PathBuf>,
    #[clap(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Subcommand)]
pub enum Cmd {
    Server(Server),
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.cmd {
            Cmd::Server(server) => {
                let settings = Settings::new(self.config)?;
                custom_tracing::init(settings.log.clone(), settings.custom_tracing.clone()).await?;
                let metrics_handle = crate::metrics::start_metrics(&settings.metrics)?;
                server.run(&settings, metrics_handle).await
            }
        }
    }
}
