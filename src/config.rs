use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    pub self_name: String,
    #[serde(deserialize_with = "deserialize_duration")]
    pub probe_interval: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub probe_timeout: Duration,
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
