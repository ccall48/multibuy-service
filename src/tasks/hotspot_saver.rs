use crate::hotspots::{Hotspots, SAVE_INTERVAL};
use std::sync::Arc;

/// Writes the hotspot registry to disk every [`SAVE_INTERVAL`] (when it has
/// changed), and once more on shutdown so a deploy loses at most nothing.
pub struct HotspotSaver {
    hotspots: Arc<Hotspots>,
}

impl HotspotSaver {
    pub fn new(hotspots: Arc<Hotspots>) -> Self {
        Self { hotspots }
    }

    async fn save(&self) {
        let hotspots = self.hotspots.clone();
        // File I/O, so off the async workers.
        match tokio::task::spawn_blocking(move || hotspots.save()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("could not save hotspot stats: {e}"),
            Err(e) => tracing::error!("hotspot save task failed: {e}"),
        }
    }

    async fn run(self, shutdown: triggered::Listener) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval(SAVE_INTERVAL);
        interval.tick().await; // the first tick is immediate; nothing to save yet

        loop {
            tokio::select! {
                biased;
                _ = shutdown.clone() => {
                    self.save().await;
                    tracing::info!("saved hotspot stats on shutdown");
                    break;
                },
                _ = interval.tick() => self.save().await,
            }
        }
        Ok(())
    }
}

impl task_manager::ManagedTask for HotspotSaver {
    fn start_task(self: Box<Self>, shutdown: triggered::Listener) -> task_manager::TaskFuture {
        task_manager::spawn(self.run(shutdown))
    }
}
