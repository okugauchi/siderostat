use serde::Deserialize;
use std::time::Duration;

fn default_heartbeat_path() -> String {
    "/v1/models".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    #[serde(alias = "self")]
    pub self_name: String,
    #[serde(default)]
    pub tls_accept_invalid_certs: bool,
    #[serde(alias = "probe_interval", deserialize_with = "deserialize_duration")]
    pub heartbeat_interval: Duration,
    #[serde(alias = "probe_timeout", deserialize_with = "deserialize_duration")]
    pub heartbeat_timeout: Duration,
    #[serde(default = "default_heartbeat_path")]
    pub heartbeat_path: String,
    #[serde(deserialize_with = "deserialize_duration")]
    pub active_probe_timeout: Duration,
    #[serde(alias = "backend")]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    pub url: String,
    pub max_in_flight: usize,
}

fn deserialize_duration<'de, D>(d: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    parse_duration(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid duration: {}", s)))
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        secs.parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>()
            .ok()
            .map(|n| Duration::from_secs(n * 60))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_config_keys() {
        let config: Config = toml::from_str(
            r#"
listen = "127.0.0.1:18080"
self_name = "macbook"
heartbeat_interval = "5s"
heartbeat_timeout = "1m"
heartbeat_path = "/v1/models"
active_probe_timeout = "3s"

[[backends]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backends]]
name = "macstudio"
url = "https://macstudio.example.internal"
max_in_flight = 2
"#,
        )
        .unwrap();

        assert_eq!(config.self_name, "macbook");
        assert!(!config.tls_accept_invalid_certs);
        assert_eq!(config.heartbeat_interval, Duration::from_secs(5));
        assert_eq!(config.heartbeat_timeout, Duration::from_secs(60));
        assert_eq!(config.heartbeat_path, "/v1/models");
        assert_eq!(config.active_probe_timeout, Duration::from_secs(3));
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[1].url, "https://macstudio.example.internal");
    }

    #[test]
    fn parses_tls_accept_invalid_certs() {
        let config: Config = toml::from_str(
            r#"
listen = "127.0.0.1:18080"
self_name = "macbook"
tls_accept_invalid_certs = true
heartbeat_interval = "5s"
heartbeat_timeout = "5s"
active_probe_timeout = "3s"

[[backends]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1
"#,
        )
        .unwrap();

        assert!(config.tls_accept_invalid_certs);
    }

    #[test]
    fn parses_legacy_config_keys() {
        let config: Config = toml::from_str(
            r#"
listen = "127.0.0.1:18080"
self = "macbook"
probe_interval = "5s"
probe_timeout = "5s"
active_probe_timeout = "3s"

[[backend]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1
"#,
        )
        .unwrap();

        assert_eq!(config.self_name, "macbook");
        assert_eq!(config.heartbeat_interval, Duration::from_secs(5));
        assert_eq!(config.heartbeat_timeout, Duration::from_secs(5));
        assert_eq!(config.heartbeat_path, "/v1/models");
        assert_eq!(config.backends.len(), 1);
    }
}
