//! monitor.toml loading and validation.
//!
//! The monitor reads an optional TOML file from the user's home directory.
//! Missing files use defaults; unknown fields are rejected to catch typos
//! (same policy as the siderostat config).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::PathBuf, time::Duration};

const DEFAULT_CONFIG_FILE: &str = "monitor.toml";
const DEFAULT_ADMIN_LISTEN: &str = "http://127.0.0.1:18081";

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LiveMetric {
    PrefillPercent,
    PrefillChunkTps,
    #[default]
    PrefillAvgTps,
    PrefillElapsed,
    DecodeChunkTps,
    DecodeAvgTps,
    DecodeElapsed,
    KvCache,
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub admin_listen: String,
    pub poll_interval_secs: u64,
    pub offline_backoff_secs: u64,
    pub show_decode_tps: bool,
    pub live_metric: LiveMetric,
    pub admin_token: Option<String>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            admin_listen: DEFAULT_ADMIN_LISTEN.to_string(),
            poll_interval_secs: 2,
            offline_backoff_secs: 5,
            show_decode_tps: true,
            live_metric: LiveMetric::default(),
            admin_token: None,
        }
    }
}

impl MonitorConfig {
    /// Load the configuration from the home directory, falling back to defaults
    /// when the file is absent. Returns the config and the resolved path.
    pub fn load() -> Result<(Self, Option<PathBuf>)> {
        let Some(path) = default_config_path() else {
            return Ok((Self::default(), None));
        };
        if !path.is_file() {
            return Ok((Self::default(), Some(path)));
        }
        let contents =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config = Self::parse(&contents)?;
        config.validate()?;
        Ok((config, Some(path)))
    }

    /// Parse configuration from TOML text.
    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).map_err(Into::into)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.poll_interval_secs > 0,
            "poll_interval_secs must be greater than zero"
        );
        anyhow::ensure!(
            self.offline_backoff_secs > 0,
            "offline_backoff_secs must be greater than zero"
        );
        anyhow::ensure!(
            self.admin_listen.starts_with("http://") || self.admin_listen.starts_with("https://"),
            "admin_listen must be an http(s) URL"
        );
        Ok(())
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub fn offline_backoff(&self) -> Duration {
        Duration::from_secs(self.offline_backoff_secs)
    }
}

fn default_config_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(DEFAULT_CONFIG_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_used_when_file_is_absent() {
        let (config, path) = MonitorConfig::load().unwrap();
        assert_eq!(config.admin_listen, DEFAULT_ADMIN_LISTEN);
        assert_eq!(config.poll_interval_secs, 2);
        assert_eq!(config.offline_backoff_secs, 5);
        assert!(config.show_decode_tps);
        assert_eq!(config.live_metric, LiveMetric::PrefillAvgTps);
        assert!(path.is_some());
    }

    #[test]
    fn parses_complete_monitor_config() {
        let config = MonitorConfig::parse(
            r#"
admin_listen = "http://127.0.0.1:18081"
poll_interval_secs = 3
offline_backoff_secs = 7
show_decode_tps = false
live_metric = "decode-chunk-tps"
admin_token = "secret"
"#,
        )
        .unwrap();
        assert_eq!(config.admin_listen, "http://127.0.0.1:18081");
        assert_eq!(config.poll_interval_secs, 3);
        assert_eq!(config.offline_backoff_secs, 7);
        assert!(!config.show_decode_tps);
        assert_eq!(config.live_metric, LiveMetric::DecodeChunkTps);
        assert_eq!(config.admin_token.as_deref(), Some("secret"));
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = MonitorConfig::parse("unknown_field = 1\n").unwrap_err();
        assert!(error.to_string().contains("unknown_field"));
    }

    #[test]
    fn rejects_zero_intervals_and_bad_url() {
        let config = MonitorConfig {
            poll_interval_secs: 0,
            ..MonitorConfig::default()
        };
        assert!(config.validate().is_err());

        let config = MonitorConfig {
            admin_listen: "127.0.0.1:18081".into(),
            ..MonitorConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
