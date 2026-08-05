use super::{
    AuthError, AuthenticatedPeer, ControlAuthenticator, SignedControlHeaders,
    auth::CONTROL_BODY_LIMIT,
};
use std::net::IpAddr;

pub const HEADER_NODE: &str = "X-DS4-Cluster-Node";
pub const HEADER_TIMESTAMP: &str = "X-DS4-Cluster-Timestamp";
pub const HEADER_NONCE: &str = "X-DS4-Cluster-Nonce";
pub const HEADER_SIGNATURE: &str = "X-DS4-Cluster-Signature";

#[derive(Default)]
pub struct BoundedControlBody {
    bytes: Vec<u8>,
}

impl BoundedControlBody {
    pub fn extend(&mut self, chunk: &[u8]) -> Result<(), AuthError> {
        let next_len = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(AuthError::BodyTooLarge)?;
        if next_len > CONTROL_BODY_LIMIT {
            return Err(AuthError::BodyTooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEndpoint {
    Node,
    Pair,
    PrepareWorker,
    BeginDrain,
    Drained,
    CancelGeneration,
    WorkerEvent,
    Demote,
}

impl ControlEndpoint {
    pub fn parse(method: &str, path: &str) -> Option<Self> {
        match (method, path) {
            ("GET", "/v1/node") => Some(Self::Node),
            ("POST", "/v1/pair") => Some(Self::Pair),
            ("POST", "/v1/prepare-worker") => Some(Self::PrepareWorker),
            ("POST", "/v1/begin-drain") => Some(Self::BeginDrain),
            ("POST", "/v1/drained") => Some(Self::Drained),
            ("POST", "/v1/cancel-generation") => Some(Self::CancelGeneration),
            ("POST", "/v1/worker-event") => Some(Self::WorkerEvent),
            ("POST", "/v1/demote") => Some(Self::Demote),
            _ => None,
        }
    }
}

pub struct ControlRequest<'a> {
    pub method: &'a str,
    pub path_and_query: &'a str,
    pub body: &'a [u8],
    pub source_ip: IpAddr,
    pub headers: &'a SignedControlHeaders,
}

impl ControlRequest<'_> {
    pub fn authenticate(
        &self,
        authenticator: &ControlAuthenticator,
        now_millis: u64,
    ) -> Result<AuthenticatedPeer, AuthError> {
        authenticator.verify(
            self.method,
            self.path_and_query,
            self.body,
            self.source_ip,
            self.headers,
            now_millis,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_the_control_protocol_method_and_path_pairs() {
        assert_eq!(
            ControlEndpoint::parse("GET", "/v1/node"),
            Some(ControlEndpoint::Node)
        );
        assert_eq!(
            ControlEndpoint::parse("POST", "/v1/prepare-worker"),
            Some(ControlEndpoint::PrepareWorker)
        );
        assert_eq!(ControlEndpoint::parse("POST", "/v1/node"), None);
        assert_eq!(ControlEndpoint::parse("GET", "/v1/pair"), None);
    }

    #[test]
    fn bounded_body_rejects_a_chunk_before_exceeding_64_kib() {
        let mut body = BoundedControlBody::default();
        body.extend(&vec![0; CONTROL_BODY_LIMIT]).unwrap();
        assert_eq!(body.extend(&[0]), Err(AuthError::BodyTooLarge));
        assert_eq!(body.as_bytes().len(), CONTROL_BODY_LIMIT);
    }
}
