use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, net::IpAddr, sync::Mutex};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

pub const CONTROL_BODY_LIMIT: usize = 64 * 1024;
pub const CONTROL_CLOCK_SKEW_MILLIS: u64 = 30_000;
pub const CONTROL_NONCE_TTL_MILLIS: u64 = 5 * 60_000;

pub struct ControlSecret(Vec<u8>);

impl ControlSecret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, AuthError> {
        let bytes = bytes.into();
        if bytes.len() < 32 {
            return Err(AuthError::InvalidSecret);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for ControlSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ControlSecret([REDACTED])")
    }
}

impl Drop for ControlSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SignedControlHeaders {
    node_id: String,
    timestamp_millis: u64,
    nonce: String,
    signature: String,
}

impl SignedControlHeaders {
    pub fn from_header_values(
        node_id: impl Into<String>,
        timestamp_millis: &str,
        nonce: impl Into<String>,
        signature: impl Into<String>,
    ) -> Result<Self, AuthError> {
        let timestamp_millis = timestamp_millis
            .parse()
            .map_err(|_| AuthError::InvalidTimestamp)?;
        Ok(Self {
            node_id: node_id.into(),
            timestamp_millis,
            nonce: nonce.into(),
            signature: signature.into(),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn timestamp_millis(&self) -> u64 {
        self.timestamp_millis
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }
}

impl fmt::Debug for SignedControlHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedControlHeaders")
            .field("node_id", &self.node_id)
            .field("timestamp_millis", &self.timestamp_millis)
            .field("nonce", &"[REDACTED]")
            .field("signature", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    node_id: String,
    source_ip: IpAddr,
    timestamp_millis: u64,
}

impl AuthenticatedPeer {
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn source_ip(&self) -> IpAddr {
        self.source_ip
    }

    pub fn timestamp_millis(&self) -> u64 {
        self.timestamp_millis
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        node_id: impl Into<String>,
        source_ip: IpAddr,
        timestamp_millis: u64,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            source_ip,
            timestamp_millis,
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    #[error("control secret must contain at least 32 bytes")]
    InvalidSecret,
    #[error("control request body exceeds the configured limit")]
    BodyTooLarge,
    #[error("control request has invalid canonical fields")]
    InvalidCanonicalFields,
    #[error("control request nonce is invalid")]
    InvalidNonce,
    #[error("control request timestamp is outside the allowed clock skew")]
    ClockSkew,
    #[error("control request timestamp is invalid")]
    InvalidTimestamp,
    #[error("control request node is not the expected peer")]
    WrongNode,
    #[error("control request source is not the expected peer address")]
    WrongSource,
    #[error("control request signature is invalid")]
    InvalidSignature,
    #[error("control request nonce has already been used")]
    Replay,
    #[error("control authentication state is unavailable")]
    Unavailable,
}

#[derive(Debug, Clone)]
struct ExpectedPeer {
    node_id: Option<String>,
    source_ip: IpAddr,
}

pub struct ControlAuthenticator {
    secret: ControlSecret,
    expected: ExpectedPeer,
    nonce_expiry: Mutex<BTreeMap<(String, String), u64>>,
}

impl fmt::Debug for ControlAuthenticator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlAuthenticator")
            .field("secret", &self.secret)
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

impl ControlAuthenticator {
    pub fn new(
        secret: ControlSecret,
        expected_node_id: impl Into<String>,
        expected_source_ip: IpAddr,
    ) -> Self {
        Self {
            secret,
            expected: ExpectedPeer {
                node_id: Some(expected_node_id.into()),
                source_ip: expected_source_ip,
            },
            nonce_expiry: Mutex::new(BTreeMap::new()),
        }
    }

    /// Authenticates the sole peer reachable at a fixed cluster address. The signed node ID is
    /// subsequently bound to the paired descriptor by `ControlProcessor`.
    pub fn new_at_source(secret: ControlSecret, expected_source_ip: IpAddr) -> Self {
        Self {
            secret,
            expected: ExpectedPeer {
                node_id: None,
                source_ip: expected_source_ip,
            },
            nonce_expiry: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn sign(
        &self,
        node_id: impl Into<String>,
        method: &str,
        path_and_query: &str,
        timestamp_millis: u64,
        nonce: impl Into<String>,
        body: &[u8],
    ) -> Result<SignedControlHeaders, AuthError> {
        validate_body_and_fields(method, path_and_query, body)?;
        let nonce = nonce.into();
        validate_nonce(&nonce)?;
        let signature = signature(
            &self.secret,
            method,
            path_and_query,
            timestamp_millis,
            &nonce,
            body,
        );
        Ok(SignedControlHeaders {
            node_id: node_id.into(),
            timestamp_millis,
            nonce,
            signature: encode_lower_hex(&signature),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        source_ip: IpAddr,
        headers: &SignedControlHeaders,
        now_millis: u64,
    ) -> Result<AuthenticatedPeer, AuthError> {
        validate_body_and_fields(method, path_and_query, body)?;
        validate_nonce(&headers.nonce)?;
        if self
            .expected
            .node_id
            .as_ref()
            .is_some_and(|expected| headers.node_id != *expected)
        {
            return Err(AuthError::WrongNode);
        }
        if source_ip != self.expected.source_ip {
            return Err(AuthError::WrongSource);
        }
        if headers.timestamp_millis.abs_diff(now_millis) > CONTROL_CLOCK_SKEW_MILLIS {
            return Err(AuthError::ClockSkew);
        }

        let supplied = decode_lower_hex_32(&headers.signature)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.secret.0).expect("HMAC accepts keys of every length");
        mac.update(&canonical_message(
            method,
            path_and_query,
            headers.timestamp_millis,
            &headers.nonce,
            body,
        ));
        mac.verify_slice(&supplied)
            .map_err(|_| AuthError::InvalidSignature)?;

        let key = (headers.node_id.clone(), headers.nonce.clone());
        let mut nonces = self
            .nonce_expiry
            .lock()
            .map_err(|_| AuthError::Unavailable)?;
        nonces.retain(|_, expires_at| *expires_at > now_millis);
        if nonces.contains_key(&key) {
            return Err(AuthError::Replay);
        }
        nonces.insert(key, now_millis.saturating_add(CONTROL_NONCE_TTL_MILLIS));

        Ok(AuthenticatedPeer {
            node_id: headers.node_id.clone(),
            source_ip,
            timestamp_millis: headers.timestamp_millis,
        })
    }
}

fn validate_body_and_fields(
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Result<(), AuthError> {
    if body.len() > CONTROL_BODY_LIMIT {
        return Err(AuthError::BodyTooLarge);
    }
    if method.is_empty()
        || !method.bytes().all(|value| value.is_ascii_uppercase())
        || !path_and_query.starts_with('/')
        || method.contains(['\r', '\n'])
        || path_and_query.contains(['\r', '\n'])
    {
        return Err(AuthError::InvalidCanonicalFields);
    }
    Ok(())
}

fn validate_nonce(nonce: &str) -> Result<(), AuthError> {
    let valid = (16..=128).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(AuthError::InvalidNonce)
    }
}

fn signature(
    secret: &ControlSecret,
    method: &str,
    path_and_query: &str,
    timestamp_millis: u64,
    nonce: &str,
    body: &[u8],
) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(&secret.0).expect("HMAC accepts keys of every length");
    mac.update(&canonical_message(
        method,
        path_and_query,
        timestamp_millis,
        nonce,
        body,
    ));
    mac.finalize().into_bytes().into()
}

fn canonical_message(
    method: &str,
    path_and_query: &str,
    timestamp_millis: u64,
    nonce: &str,
    body: &[u8],
) -> Vec<u8> {
    let body_digest = Sha256::digest(body);
    format!(
        "{method}\n{path_and_query}\n{timestamp_millis}\n{nonce}\n{}",
        encode_lower_hex(&body_digest)
    )
    .into_bytes()
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn decode_lower_hex_32(value: &str) -> Result<[u8; 32], AuthError> {
    if value.len() != 64 {
        return Err(AuthError::InvalidSignature);
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = lower_hex_value(pair[0]).ok_or(AuthError::InvalidSignature)?;
        let low = lower_hex_value(pair[1]).ok_or(AuthError::InvalidSignature)?;
        output[index] = high << 4 | low;
    }
    Ok(output)
}

fn lower_hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn authenticator() -> ControlAuthenticator {
        ControlAuthenticator::new(
            ControlSecret::new(vec![0x5a; 32]).unwrap(),
            "peer-node",
            "10.99.0.2".parse().unwrap(),
        )
    }

    fn signed(auth: &ControlAuthenticator) -> SignedControlHeaders {
        auth.sign(
            "peer-node",
            "POST",
            "/v1/pair?generation=7",
            NOW,
            "nonce-0000000001",
            br#"{"generation":7}"#,
        )
        .unwrap()
    }

    #[test]
    fn authenticates_once_and_rejects_replay() {
        let auth = authenticator();
        let headers = signed(&auth);
        let verify = || {
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &headers,
                NOW,
            )
        };
        assert!(verify().is_ok());
        assert_eq!(verify(), Err(AuthError::Replay));
    }

    #[test]
    fn retains_nonce_for_five_minutes_then_expires_it() {
        let auth = authenticator();
        let first = signed(&auth);
        assert!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &first,
                NOW,
            )
            .is_ok()
        );
        let within_ttl = auth
            .sign(
                "peer-node",
                "POST",
                "/v1/pair?generation=7",
                NOW + 120_000,
                "nonce-0000000001",
                br#"{"generation":7}"#,
            )
            .unwrap();
        assert_eq!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &within_ttl,
                NOW + 120_000,
            ),
            Err(AuthError::Replay)
        );
        let after_ttl = auth
            .sign(
                "peer-node",
                "POST",
                "/v1/pair?generation=7",
                NOW + CONTROL_NONCE_TTL_MILLIS,
                "nonce-0000000001",
                br#"{"generation":7}"#,
            )
            .unwrap();
        assert!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &after_ttl,
                NOW + CONTROL_NONCE_TTL_MILLIS,
            )
            .is_ok()
        );
    }

    #[test]
    fn canonical_signature_matches_known_vector_and_received_headers_parse() {
        let auth = authenticator();
        let headers = signed(&auth);
        assert_eq!(
            headers.signature(),
            "19528c839862cd4d37086e417539998dd92985ddf4358d53a2097f480c2576ef"
        );
        assert_eq!(
            SignedControlHeaders::from_header_values(
                headers.node_id(),
                &headers.timestamp_millis().to_string(),
                headers.nonce(),
                headers.signature(),
            ),
            Ok(headers)
        );
        assert_eq!(
            SignedControlHeaders::from_header_values("peer", "not-a-time", "nonce", "signature"),
            Err(AuthError::InvalidTimestamp)
        );
    }

    #[test]
    fn rejects_clock_skew_wrong_source_and_wrong_node() {
        let auth = authenticator();
        let mut headers = signed(&auth);
        assert_eq!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &headers,
                NOW + CONTROL_CLOCK_SKEW_MILLIS + 1,
            ),
            Err(AuthError::ClockSkew)
        );
        assert_eq!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "192.168.1.8".parse().unwrap(),
                &headers,
                NOW,
            ),
            Err(AuthError::WrongSource)
        );
        headers.node_id = "other-node".into();
        assert_eq!(
            auth.verify(
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                "10.99.0.2".parse().unwrap(),
                &headers,
                NOW,
            ),
            Err(AuthError::WrongNode)
        );
    }

    #[test]
    fn every_signed_field_mutation_fails() {
        type Case = (&'static str, &'static str, &'static [u8], u64, &'static str);
        let cases: [Case; 5] = [
            (
                "GET",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                NOW,
                "nonce-0000000001",
            ),
            (
                "POST",
                "/v1/node",
                br#"{"generation":7}"#,
                NOW,
                "nonce-0000000001",
            ),
            (
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":8}"#,
                NOW,
                "nonce-0000000001",
            ),
            (
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                NOW + 1,
                "nonce-0000000001",
            ),
            (
                "POST",
                "/v1/pair?generation=7",
                br#"{"generation":7}"#,
                NOW,
                "nonce-0000000002",
            ),
        ];
        for (method, path, body, timestamp, nonce) in cases {
            let auth = authenticator();
            let mut headers = signed(&auth);
            headers.timestamp_millis = timestamp;
            headers.nonce = nonce.into();
            assert_eq!(
                auth.verify(
                    method,
                    path,
                    body,
                    "10.99.0.2".parse().unwrap(),
                    &headers,
                    NOW,
                ),
                Err(AuthError::InvalidSignature)
            );
        }
    }

    #[test]
    fn enforces_body_limit_and_redacts_sensitive_debug_fields() {
        let auth = authenticator();
        assert_eq!(
            auth.sign(
                "peer-node",
                "POST",
                "/v1/pair",
                NOW,
                "nonce-0000000001",
                &vec![0; CONTROL_BODY_LIMIT + 1],
            ),
            Err(AuthError::BodyTooLarge)
        );
        let debug = format!("{:?} {:?}", auth, signed(&auth));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("5a5a5a"));
        assert!(!debug.contains("nonce-0000000001"));
        assert!(!debug.contains(signed(&auth).signature()));
    }
}
