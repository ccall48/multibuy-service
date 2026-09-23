use config::{Config, Environment, File};
use humantime_serde::re::humantime;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Settings {
    /// Log level configuration (RUST_LOG compatible)
    #[serde(default = "default_log")]
    pub log: String,

    /// Custom tracing settings for dynamic log level updates
    #[serde(default)]
    pub custom_tracing: custom_tracing::Settings,
    /// Listen address for grpc requests. Default "0.0.0.0:6080"
    #[serde(default = "default_grpc_listen_addr")]
    pub grpc_listen: SocketAddr,
    /// Metrics settings
    #[serde(default)]
    pub metrics: crate::metrics::Settings,
    /// Admin API and dashboard settings
    #[serde(default)]
    pub api: crate::api::Settings,
    #[serde(default = "default_cleanup_timeout", with = "humantime_serde")]
    pub cleanup_timeout: Duration,
    /// Base58-encoded hotspot public keys to deny
    #[serde(default)]
    pub denied_hotspots: Vec<String>,
    /// Region names to deny (e.g., "US915", "EU868")
    #[serde(default)]
    pub denied_regions: Vec<String>,
}

pub fn default_log() -> String {
    "INFO".to_string()
}

pub fn default_grpc_listen_addr() -> SocketAddr {
    "0.0.0.0:6080".parse().expect("invalid default socket addr")
}

pub fn default_cleanup_timeout() -> Duration {
    humantime::parse_duration("30 minutes").unwrap()
}

impl Settings {
    /// Load Settings from a given path. Settings are loaded from a given
    /// optional path and can be overriden with environment variables.
    ///
    /// Environemnt overrides have the same name as the entries in the settings
    /// file in uppercase and prefixed with "MB__". For example
    /// "MB__LOG" will override the log setting. Nested entries use "__" between
    /// levels, so "MB__API__LISTEN" overrides `api.listen`.
    pub fn new<P: AsRef<Path>>(path: Option<P>) -> Result<Self, config::ConfigError> {
        let mut builder = Config::builder();

        if let Some(file) = path {
            // Add optional settings file
            let filename = file.as_ref().to_str().expect("file name");
            builder = builder.add_source(File::with_name(filename).required(false));
        }
        // Add in settings from the environment (with a prefix of MB)
        // Eg.. `MB__LOG=DEBUG ./target/release/multi_buy_service` would set the `log` key
        builder
            .add_source(
                Environment::with_prefix("MB")
                    .prefix_separator("__")
                    // Without this, nested keys like MB__METRICS__ENDPOINT are
                    // read as the flat key "metrics__endpoint" and silently
                    // ignored.
                    .separator("__")
                    .try_parsing(true)
                    .list_separator(",")
                    .with_list_parse_key("denied_hotspots")
                    .with_list_parse_key("denied_regions"),
            )
            .build()
            .and_then(|config| config.try_deserialize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Mutex to prevent env var tests from racing each other.
    // Using .lock().unwrap_or_else to recover from poison.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn with_env_vars<F: FnOnce() -> R, R>(vars: &[(&str, &str)], f: F) -> R {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        for (key, val) in vars {
            unsafe { std::env::set_var(key, val) };
        }
        let result = f();
        for (key, _) in vars {
            unsafe { std::env::remove_var(key) };
        }
        result
    }

    #[test]
    fn single_denied_region_from_env() {
        with_env_vars(&[("MB__DENIED_REGIONS", "EU868")], || {
            let settings = Settings::new::<String>(None).unwrap();
            assert_eq!(settings.denied_regions, vec!["EU868"]);
        });
    }

    #[test]
    fn multiple_denied_regions_from_env() {
        with_env_vars(&[("MB__DENIED_REGIONS", "EU868,US915")], || {
            let settings = Settings::new::<String>(None).unwrap();
            assert_eq!(settings.denied_regions, vec!["EU868", "US915"]);
        });
    }

    #[test]
    fn single_denied_hotspot_from_env() {
        with_env_vars(
            &[(
                "MB__DENIED_HOTSPOTS",
                "112bUuQaE7j73THS9ABShHGokm46Miip9L361FSyWv7zSYn8hZWf",
            )],
            || {
                let settings = Settings::new::<String>(None).unwrap();
                assert_eq!(
                    settings.denied_hotspots,
                    vec!["112bUuQaE7j73THS9ABShHGokm46Miip9L361FSyWv7zSYn8hZWf"]
                );
            },
        );
    }

    #[test]
    fn multiple_denied_hotspots_from_env() {
        with_env_vars(&[("MB__DENIED_HOTSPOTS", "key1,key2,key3")], || {
            let settings = Settings::new::<String>(None).unwrap();
            assert_eq!(settings.denied_hotspots, vec!["key1", "key2", "key3"]);
        });
    }

    #[test]
    fn nested_settings_from_env() {
        with_env_vars(
            &[
                ("MB__METRICS__ENDPOINT", "127.0.0.1:19911"),
                ("MB__API__LISTEN", "127.0.0.1:16081"),
                ("MB__API__AUTH_TOKEN", "s3cret"),
                ("MB__API__ENABLED", "false"),
            ],
            || {
                let settings = Settings::new::<String>(None).unwrap();
                assert_eq!(settings.metrics.endpoint.to_string(), "127.0.0.1:19911");
                assert_eq!(settings.api.listen.to_string(), "127.0.0.1:16081");
                assert_eq!(settings.api.auth_token.as_deref(), Some("s3cret"));
                assert!(!settings.api.enabled);
            },
        );
    }

    #[test]
    fn api_defaults() {
        with_env_vars(&[], || {
            let settings = Settings::new::<String>(None).unwrap();
            assert!(settings.api.enabled);
            assert_eq!(settings.api.listen.to_string(), "0.0.0.0:6081");
            assert!(settings.api.auth_token.is_none());
        });
    }

    #[test]
    fn empty_deny_lists_by_default() {
        with_env_vars(&[], || {
            let settings = Settings::new::<String>(None).unwrap();
            assert!(settings.denied_regions.is_empty());
            assert!(settings.denied_hotspots.is_empty());
        });
    }
}
