use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Settings {
    /// Whether to run the admin API and dashboard at all.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Listen address for the admin API and dashboard.
    #[serde(default = "default_api_listen")]
    pub listen: SocketAddr,
    /// Shared secret required in `Authorization: Bearer <token>` on API
    /// requests. When unset the API is unauthenticated, so bind `listen` to an
    /// address only trusted operators can reach.
    #[serde(default)]
    pub auth_token: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            listen: default_api_listen(),
            auth_token: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_api_listen() -> SocketAddr {
    "0.0.0.0:6081".parse().expect("invalid default socket addr")
}
