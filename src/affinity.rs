use crate::{
    config::AffinityConfig,
    error::ProxyError,
    persistence::{PersistedAffinity, Persistence, unix_now},
};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, env, sync::RwLock, time::Duration};
use tracing::error;
use unicode_normalization::UnicodeNormalization;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AffinitySource {
    Explicit = 1,
    HermesSession = 2,
    HermesKey = 3,
    Conversation = 4,
    PreviousResponse = 5,
    Prefix = 6,
    BodySession = 7,
}

impl AffinitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::HermesSession => "hermes_session",
            Self::HermesKey => "hermes_key",
            Self::Conversation => "conversation",
            Self::PreviousResponse => "previous_response",
            Self::Prefix => "prefix",
            Self::BodySession => "body_session",
        }
    }

    fn namespace(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::HermesSession => "hermes-session",
            Self::HermesKey => "hermes-key",
            Self::Conversation => "conversation",
            Self::PreviousResponse => "body-previous-response",
            Self::Prefix => "prefix-sha256",
            Self::BodySession => "body-session",
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Explicit,
            2 => Self::HermesSession,
            3 => Self::HermesKey,
            4 => Self::Conversation,
            5 => Self::PreviousResponse,
            6 => Self::Prefix,
            7 => Self::BodySession,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AffinityKey {
    pub hash: [u8; 32],
    pub source: AffinitySource,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct AffinityEntry {
    pub key_hash: [u8; 32],
    pub source: AffinitySource,
    pub backend_id: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub absolute_expires_at: i64,
    pub assignment_generation: u64,
    pub failure_count: u32,
}

pub struct AffinityStore {
    config: AffinityConfig,
    secret: Vec<u8>,
    entries: RwLock<HashMap<[u8; 32], AffinityEntry>>,
    persistence: Option<Persistence>,
    persistence_start_failed: bool,
}

impl AffinityStore {
    pub fn new(config: &AffinityConfig) -> anyhow::Result<Self> {
        let secret = if config.enabled {
            env::var(&config.secret_env)?.into_bytes()
        } else {
            Vec::new()
        };
        Self::with_secret(config, secret)
    }

    pub(crate) fn with_secret(config: &AffinityConfig, secret: Vec<u8>) -> anyhow::Result<Self> {
        let (persistence, persisted, persistence_start_failed) = match &config.database_path {
            Some(path) if config.enabled => match Persistence::open(path) {
                Ok((persistence, entries)) => (Some(persistence), entries, false),
                Err(open_error) => {
                    error!(
                        error = %open_error,
                        "affinity persistence is unavailable; routing will continue in memory"
                    );
                    (None, Vec::new(), true)
                }
            },
            _ => (None, Vec::new(), false),
        };
        let entries = persisted
            .into_iter()
            .filter_map(from_persisted)
            .map(|entry| (entry.key_hash, entry))
            .collect();
        Ok(Self {
            config: config.clone(),
            secret,
            entries: RwLock::new(entries),
            persistence,
            persistence_start_failed,
        })
    }

    pub fn extract(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<Option<AffinityKey>, ProxyError> {
        if !self.config.enabled {
            return Ok(None);
        }

        const HEADERS: &[(&str, AffinitySource, bool)] = &[
            ("x-ds4-affinity-key", AffinitySource::Explicit, true),
            ("x-hermes-session-id", AffinitySource::HermesSession, false),
            ("x-hermes-session-key", AffinitySource::HermesKey, false),
            ("x-ds4-conversation-id", AffinitySource::Conversation, true),
            ("x-conversation-id", AffinitySource::Conversation, false),
            ("conversation-id", AffinitySource::Conversation, false),
            ("session-id", AffinitySource::BodySession, false),
            ("x-session-id", AffinitySource::BodySession, false),
        ];

        for (name, source, strict) in HEADERS {
            let Some(value) = headers.get(*name) else {
                continue;
            };
            let normalized = value.to_str().ok().and_then(|value| normalize(value).ok());
            match normalized {
                Some(value) => return Ok(Some(self.key(*source, &value))),
                None if *strict => return Err(ProxyError::InvalidAffinity),
                None => continue,
            }
        }

        if let Ok(json) = serde_json::from_slice::<Value>(body) {
            if let Some(value) = conversation_id(&json)
                && let Ok(value) = normalize(value)
            {
                return Ok(Some(self.key(AffinitySource::Conversation, &value)));
            }
            if let Some(value) = json.get("previous_response_id").and_then(Value::as_str)
                && let Ok(value) = normalize(value)
            {
                return Ok(Some(self.key(AffinitySource::PreviousResponse, &value)));
            }
            if self.config.allow_body_ids {
                for name in ["session_id", "conversation_id"] {
                    if let Some(value) = json.get(name).and_then(Value::as_str)
                        && let Ok(value) = normalize(value)
                    {
                        return Ok(Some(self.key(AffinitySource::BodySession, &value)));
                    }
                }
            }
        }

        if let Some(value) = headers.get("x-ds4-prefix-hash") {
            let value = value.to_str().map_err(|_| ProxyError::InvalidAffinity)?;
            validate_prefix_hash(value)?;
            return Ok(Some(self.key(AffinitySource::Prefix, value)));
        }
        if self.config.compute_prefix_affinity
            && body.len() >= self.config.minimum_prefix_bytes
            && let Some(prefix_hash) =
                computed_prefix_hash(body, self.config.maximum_prefix_hash_bytes)
        {
            return Ok(Some(self.key(AffinitySource::Prefix, &prefix_hash)));
        }
        Ok(None)
    }

    pub fn lookup(&self, key: &AffinityKey) -> Option<AffinityEntry> {
        let now = unix_now();
        let mut entries = self.entries.write().ok()?;
        let entry = entries.get_mut(&key.hash)?;
        if entry.expires_at <= now || entry.absolute_expires_at <= now {
            entries.remove(&key.hash);
            if let Some(persistence) = &self.persistence {
                persistence.delete(key.hash);
            }
            return None;
        }
        let sliding = ttl_for_source(&self.config, entry.source).0.as_secs() as i64;
        entry.last_seen_at = now;
        entry.expires_at = (now + sliding).min(entry.absolute_expires_at);
        let result = entry.clone();
        if let Some(persistence) = &self.persistence {
            persistence.upsert(to_persisted(&result));
        }
        Some(result)
    }

    pub fn assign(&self, key: &AffinityKey, backend_id: &str) {
        if !self.config.enabled {
            return;
        }
        let now = unix_now();
        let (sliding, absolute) = ttl_for_source(&self.config, key.source);
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => return,
        };
        if entries.len() >= self.config.max_entries
            && !entries.contains_key(&key.hash)
            && let Some(oldest) = entries
                .values()
                .min_by_key(|entry| entry.last_seen_at)
                .map(|entry| entry.key_hash)
        {
            entries.remove(&oldest);
            if let Some(persistence) = &self.persistence {
                persistence.delete(oldest);
            }
        }
        let generation = entries
            .get(&key.hash)
            .map_or(1, |entry| entry.assignment_generation.saturating_add(1));
        let entry = AffinityEntry {
            key_hash: key.hash,
            source: key.source,
            backend_id: backend_id.to_string(),
            created_at: now,
            last_seen_at: now,
            expires_at: now + sliding.as_secs() as i64,
            absolute_expires_at: now + absolute.as_secs() as i64,
            assignment_generation: generation,
            failure_count: 0,
        };
        entries.insert(key.hash, entry.clone());
        if let Some(persistence) = &self.persistence {
            persistence.upsert(to_persisted(&entry));
        }
    }

    pub fn mark_failure(&self, key: &AffinityKey) {
        if let Ok(mut entries) = self.entries.write()
            && let Some(entry) = entries.get_mut(&key.hash)
        {
            entry.failure_count = entry.failure_count.saturating_add(1);
            if let Some(persistence) = &self.persistence {
                persistence.upsert(to_persisted(entry));
            }
        }
    }

    pub fn remove(&self, key_hash: [u8; 32]) -> bool {
        let removed = self
            .entries
            .write()
            .is_ok_and(|mut entries| entries.remove(&key_hash).is_some());
        if removed && let Some(persistence) = &self.persistence {
            persistence.delete(key_hash);
        }
        removed
    }

    pub fn remove_by_tag(&self, tag: &str) -> bool {
        if tag.len() != 12 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        let matches = self.entries.read().map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .keys()
                    .filter(|hash| hex_prefix(hash.as_slice(), 6).eq_ignore_ascii_case(tag))
                    .copied()
                    .collect::<Vec<_>>()
            },
        );
        if matches.len() == 1 {
            self.remove(matches[0])
        } else {
            false
        }
    }

    pub fn cleanup(&self) {
        let now = unix_now();
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|_, entry| entry.expires_at > now && entry.absolute_expires_at > now);
        }
        if let Some(persistence) = &self.persistence {
            persistence.delete_expired(now);
        }
    }

    pub fn counts_by_backend(&self) -> HashMap<String, usize> {
        self.entries.read().map_or_else(
            |_| HashMap::new(),
            |entries| {
                let mut result = HashMap::new();
                for entry in entries.values() {
                    *result.entry(entry.backend_id.clone()).or_insert(0) += 1;
                }
                result
            },
        )
    }

    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |entries| entries.len())
    }

    pub fn persistence_healthy(&self) -> bool {
        !self.persistence_start_failed
            && self
                .persistence
                .as_ref()
                .is_none_or(Persistence::is_healthy)
    }

    fn key(&self, source: AffinitySource, value: &str) -> AffinityKey {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC supports arbitrary key length");
        mac.update(source.namespace().as_bytes());
        mac.update(&[0]);
        mac.update(value.as_bytes());
        let hash: [u8; 32] = mac.finalize().into_bytes().into();
        AffinityKey {
            tag: hex_prefix(&hash, 6),
            hash,
            source,
        }
    }
}

fn normalize(value: &str) -> Result<String, ()> {
    let trimmed = value.trim_matches(|character: char| character.is_ascii_whitespace());
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err(());
    }
    if trimmed.chars().any(|character| {
        let value = character as u32;
        value <= 0x1f || (0x7f..=0x9f).contains(&value)
    }) {
        return Err(());
    }
    Ok(trimmed.nfc().collect())
}

fn conversation_id(value: &Value) -> Option<&str> {
    match value.get("conversation")? {
        Value::String(value) => Some(value),
        Value::Object(object) => object.get("id").and_then(Value::as_str),
        _ => None,
    }
}

fn validate_prefix_hash(value: &str) -> Result<(), ProxyError> {
    let value = value.trim();
    let is_hex = value.len().is_multiple_of(2)
        && value.len() >= 32
        && value.len() <= 128
        && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    let is_base64url = (22..=86).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'='));
    if is_hex || is_base64url {
        Ok(())
    } else {
        Err(ProxyError::InvalidAffinity)
    }
}

fn computed_prefix_hash(body: &[u8], maximum_bytes: usize) -> Option<String> {
    let json: Value = serde_json::from_slice(body).ok()?;
    let object = json.as_object()?;
    let mut stable = serde_json::Map::new();
    if let Some(model) = object.get("model") {
        stable.insert("model".into(), model.clone());
    }
    if let Some(instructions) = object.get("instructions") {
        stable.insert("instructions".into(), instructions.clone());
    }
    if let Some(input) = object.get("input") {
        stable.insert("input".into(), first_stable_element(input));
    }
    if let Some(messages) = object.get("messages").and_then(Value::as_array) {
        let selected = ["system", "developer", "user"]
            .iter()
            .filter_map(|role| {
                messages
                    .iter()
                    .find(|message| message.get("role").and_then(Value::as_str) == Some(*role))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !selected.is_empty() {
            stable.insert("messages".into(), Value::Array(selected));
        }
    }
    if let Some(tools) = object.get("tools") {
        stable.insert("tools".into(), tools.clone());
    }
    if stable.len() <= usize::from(object.contains_key("model")) {
        return None;
    }
    let canonical = serde_json::to_vec(&Value::Object(stable)).ok()?;
    let digest = Sha256::digest(&canonical[..canonical.len().min(maximum_bytes)]);
    Some(hex_prefix(&digest, digest.len()))
}

fn first_stable_element(value: &Value) -> Value {
    match value {
        Value::Array(values) => values.first().cloned().unwrap_or(Value::Null),
        other => other.clone(),
    }
}

fn ttl_for_source(config: &AffinityConfig, source: AffinitySource) -> (Duration, Duration) {
    match source {
        AffinitySource::HermesKey => (
            Duration::from_secs(14 * 86_400),
            Duration::from_secs(90 * 86_400),
        ),
        AffinitySource::PreviousResponse | AffinitySource::Prefix => {
            (Duration::from_secs(86_400), Duration::from_secs(7 * 86_400))
        }
        _ => (config.default_sliding_ttl, config.default_absolute_ttl),
    }
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    let mut output = String::with_capacity(count * 2);
    for byte in bytes.iter().take(count) {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn to_persisted(entry: &AffinityEntry) -> PersistedAffinity {
    PersistedAffinity {
        key_hash: entry.key_hash,
        source: entry.source as u8,
        backend_id: entry.backend_id.clone(),
        created_at: entry.created_at,
        last_seen_at: entry.last_seen_at,
        expires_at: entry.expires_at,
        absolute_expires_at: entry.absolute_expires_at,
        assignment_generation: entry.assignment_generation,
        failure_count: entry.failure_count,
    }
}

fn from_persisted(entry: PersistedAffinity) -> Option<AffinityEntry> {
    Some(AffinityEntry {
        key_hash: entry.key_hash,
        source: AffinitySource::from_u8(entry.source)?,
        backend_id: entry.backend_id,
        created_at: entry.created_at,
        last_seen_at: entry.last_seen_at,
        expires_at: entry.expires_at,
        absolute_expires_at: entry.absolute_expires_at,
        assignment_generation: entry.assignment_generation,
        failure_count: entry.failure_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn store() -> AffinityStore {
        let config = AffinityConfig {
            enabled: true,
            database_path: None,
            ..AffinityConfig::default()
        };
        AffinityStore::with_secret(&config, vec![42; 32]).unwrap()
    }

    #[test]
    fn normalization_rejects_controls_and_normalizes_unicode() {
        assert!(normalize("abc\n").is_ok()); // trailing ASCII whitespace is trimmed
        assert!(normalize("a\nb").is_err());
        assert_eq!(normalize(" e\u{301} ").unwrap(), "é");
    }

    #[test]
    fn prefix_hash_validation() {
        assert!(validate_prefix_hash("00112233445566778899aabbccddeeff").is_ok());
        assert!(validate_prefix_hash("short").is_err());
    }

    #[test]
    fn computed_prefix_ignores_volatile_request_fields() {
        let first = br#"{"model":"m","messages":[{"role":"system","content":"stable"},{"role":"user","content":"hello"}],"stream":true,"temperature":0}"#;
        let second = br#"{"temperature":1,"stream":false,"messages":[{"content":"stable","role":"system"},{"content":"hello","role":"user"}],"model":"m"}"#;
        assert_eq!(
            computed_prefix_hash(first, 4096),
            computed_prefix_hash(second, 4096)
        );
    }

    #[test]
    fn explicit_header_has_priority_and_is_namespaced() {
        let store = store();
        let mut headers = HeaderMap::new();
        headers.insert("x-ds4-affinity-key", HeaderValue::from_static("same-value"));
        headers.insert(
            "x-hermes-session-id",
            HeaderValue::from_static("same-value"),
        );
        let explicit = store.extract(&headers, b"{}").unwrap().unwrap();
        assert_eq!(explicit.source, AffinitySource::Explicit);

        headers.remove("x-ds4-affinity-key");
        let hermes = store.extract(&headers, b"{}").unwrap().unwrap();
        assert_ne!(explicit.hash, hermes.hash);
    }

    #[test]
    fn invalid_explicit_header_is_rejected_but_compatibility_header_is_skipped() {
        let store = store();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ds4-affinity-key",
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(matches!(
            store.extract(&headers, b"{}"),
            Err(ProxyError::InvalidAffinity)
        ));

        headers.remove("x-ds4-affinity-key");
        headers.insert(
            "x-hermes-session-id",
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        headers.insert("x-conversation-id", HeaderValue::from_static("valid"));
        assert_eq!(
            store.extract(&headers, b"{}").unwrap().unwrap().source,
            AffinitySource::Conversation
        );
    }
}
