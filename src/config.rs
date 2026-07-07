use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    #[serde(alias = "self")]
    pub self_name: String,
    #[serde(default)]
    pub tls_accept_invalid_certs: bool,
    #[serde(deserialize_with = "deserialize_duration")]
    pub probe_interval: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub probe_timeout: Duration,
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
self = "macbook"
probe_interval = "5s"
probe_timeout = "1m"

[[backend]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backend]]
name = "macstudio"
url = "https://macstudio.example.internal"
max_in_flight = 2
"#,
        )
        .unwrap();

        assert_eq!(config.self_name, "macbook");
        assert!(!config.tls_accept_invalid_certs);
        assert_eq!(config.probe_interval, Duration::from_secs(5));
        assert_eq!(config.probe_timeout, Duration::from_secs(60));
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[1].url, "https://macstudio.example.internal");
    }

    #[test]
    fn parses_tls_accept_invalid_certs() {
        let config: Config = toml::from_str(
            r#"
listen = "127.0.0.1:18080"
self = "macbook"
tls_accept_invalid_certs = true
probe_interval = "5s"
probe_timeout = "5s"

[[backend]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1
"#,
        )
        .unwrap();

        assert!(config.tls_accept_invalid_certs);
    }
}
