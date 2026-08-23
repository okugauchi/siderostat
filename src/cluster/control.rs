use super::{
    AuthError, AuthenticatedPeer, ControlAuthenticator, SignedControlHeaders,
    auth::CONTROL_BODY_LIMIT,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::IpAddr, time::Duration};
use thiserror::Error;

pub const HEADER_NODE: &str = "X-DS4-Cluster-Node";
pub const HEADER_TIMESTAMP: &str = "X-DS4-Cluster-Timestamp";
pub const HEADER_NONCE: &str = "X-DS4-Cluster-Nonce";
pub const HEADER_SIGNATURE: &str = "X-DS4-Cluster-Signature";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlRole {
    Coordinator,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlMode {
    SoloStandalone,
    PairedStandalone,
    #[serde(alias = "distributed-mxfp4")]
    DistributedLayerParallel,
    Transitioning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributedControlPhase {
    Unpaired,
    Paired,
    WorkerPreparing,
    WorkerReady,
    Draining,
    Drained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerEventKind {
    Ready,
    Exited,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDescriptor {
    pub protocol_version: u16,
    pub node_id: String,
    pub role: ControlRole,
    pub generation: u64,
    pub mode: ControlMode,
    pub deployment_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum ControlCommand {
    Pair { descriptor: NodeDescriptor },
    PrepareWorker,
    BeginDrain,
    Drained,
    CancelGeneration,
    WorkerEvent { event: WorkerEventKind },
    DistributedReady,
    Demote,
}

impl ControlCommand {
    pub fn endpoint(&self) -> ControlEndpoint {
        match self {
            Self::Pair { .. } => ControlEndpoint::Pair,
            Self::PrepareWorker => ControlEndpoint::PrepareWorker,
            Self::BeginDrain => ControlEndpoint::BeginDrain,
            Self::Drained => ControlEndpoint::Drained,
            Self::CancelGeneration => ControlEndpoint::CancelGeneration,
            Self::WorkerEvent { .. } => ControlEndpoint::WorkerEvent,
            Self::DistributedReady => ControlEndpoint::DistributedReady,
            Self::Demote => ControlEndpoint::Demote,
        }
    }

    fn requires_matching_deployment(&self) -> bool {
        !matches!(self, Self::Pair { .. } | Self::Demote)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlMessage {
    pub request_id: String,
    pub generation: u64,
    pub deployment_id: Option<String>,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlResponseStatus {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub status: ControlResponseStatus,
    pub generation: u64,
    pub descriptor: NodeDescriptor,
    pub lease_expires_at_millis: Option<u64>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControlError {
    #[error("control endpoint does not match message command")]
    EndpointMismatch,
    #[error("control request ID is invalid")]
    InvalidRequestId,
    #[error("control generation mismatch: expected {expected}, received {received}")]
    GenerationMismatch { expected: u64, received: u64 },
    #[error("control deployment does not match")]
    DeploymentMismatch,
    #[error("peer descriptor is invalid")]
    InvalidDescriptor,
    #[error("peer route is not scoped to the cluster interface")]
    RouteNotScoped,
    #[error("peer has not established a control lease")]
    PeerNotPaired,
    #[error("request ID was reused with different content")]
    IdempotencyConflict,
    #[error("command is not accepted by this role")]
    CommandNotAllowed,
    #[error("control command is not valid in phase {phase:?}")]
    InvalidPhase { phase: DistributedControlPhase },
}

impl ControlError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::DeploymentMismatch => 412,
            Self::GenerationMismatch { .. }
            | Self::IdempotencyConflict
            | Self::PeerNotPaired
            | Self::InvalidPhase { .. } => 409,
            Self::CommandNotAllowed => 403,
            Self::EndpointMismatch
            | Self::InvalidRequestId
            | Self::InvalidDescriptor
            | Self::RouteNotScoped => 400,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerLease {
    lease_millis: u64,
    required_stability_millis: u64,
    generation: Option<u64>,
    descriptor: Option<NodeDescriptor>,
    first_authenticated_at_millis: Option<u64>,
    expires_at_millis: Option<u64>,
    route_scoped: bool,
}

impl PeerLease {
    pub fn new(lease: Duration, required_stability: Duration) -> Self {
        Self {
            lease_millis: duration_millis(lease),
            required_stability_millis: duration_millis(required_stability),
            generation: None,
            descriptor: None,
            first_authenticated_at_millis: None,
            expires_at_millis: None,
            route_scoped: false,
        }
    }

    fn establish(
        &mut self,
        authenticated: &AuthenticatedPeer,
        descriptor: NodeDescriptor,
        generation: u64,
        route_scoped: bool,
        now_millis: u64,
    ) -> Result<(), ControlError> {
        if !route_scoped {
            return Err(ControlError::RouteNotScoped);
        }
        if descriptor.node_id != authenticated.node_id() || descriptor.generation != generation {
            return Err(ControlError::InvalidDescriptor);
        }
        let same_membership = self.generation == Some(generation)
            && self
                .descriptor
                .as_ref()
                .is_some_and(|current| current.node_id == descriptor.node_id)
            && !self.expired(now_millis);
        if !same_membership {
            self.first_authenticated_at_millis = Some(now_millis);
        }
        self.generation = Some(generation);
        self.descriptor = Some(descriptor);
        self.route_scoped = true;
        self.expires_at_millis = Some(now_millis.saturating_add(self.lease_millis));
        Ok(())
    }

    fn renew(
        &mut self,
        authenticated: &AuthenticatedPeer,
        generation: u64,
        route_scoped: bool,
        now_millis: u64,
    ) -> Result<(), ControlError> {
        if !route_scoped {
            self.route_scoped = false;
            return Err(ControlError::RouteNotScoped);
        }
        let matching = self.generation == Some(generation)
            && self
                .descriptor
                .as_ref()
                .is_some_and(|descriptor| descriptor.node_id == authenticated.node_id());
        if !matching || self.expired(now_millis) {
            return Err(ControlError::PeerNotPaired);
        }
        self.route_scoped = true;
        self.expires_at_millis = Some(now_millis.saturating_add(self.lease_millis));
        Ok(())
    }

    pub fn peer_present(&self, now_millis: u64) -> bool {
        let stable = self.first_authenticated_at_millis.is_some_and(|first| {
            now_millis >= first.saturating_add(self.required_stability_millis)
        });
        self.route_scoped && stable && !self.expired(now_millis) && self.descriptor.is_some()
    }

    pub fn expired(&self, now_millis: u64) -> bool {
        self.expires_at_millis
            .is_none_or(|expires_at| now_millis >= expires_at)
    }

    pub fn expires_at_millis(&self) -> Option<u64> {
        self.expires_at_millis
    }

    pub fn descriptor(&self) -> Option<&NodeDescriptor> {
        self.descriptor.as_ref()
    }

    fn matches_peer(&self, authenticated: &AuthenticatedPeer) -> bool {
        self.descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.node_id == authenticated.node_id())
    }

    pub fn route_scoped(&self) -> bool {
        self.route_scoped
    }

    pub fn invalidate_route(&mut self) {
        self.route_scoped = false;
    }

    fn advance_generation(&mut self, generation: u64) {
        self.generation = self.generation.map(|_| generation);
        if let Some(descriptor) = &mut self.descriptor {
            descriptor.generation = generation;
        }
    }
}

#[derive(Debug)]
pub(crate) struct ControlProcessor {
    local: NodeDescriptor,
    expected_peer_role: ControlRole,
    lease: PeerLease,
    processed: BTreeMap<(u64, String), ControlMessage>,
}

impl ControlProcessor {
    pub(crate) fn new(
        local: NodeDescriptor,
        expected_peer_role: ControlRole,
        lease: Duration,
        required_stability: Duration,
    ) -> Self {
        Self {
            local,
            expected_peer_role,
            lease: PeerLease::new(lease, required_stability),
            processed: BTreeMap::new(),
        }
    }

    pub(crate) fn descriptor_response(
        &mut self,
        authenticated: &AuthenticatedPeer,
        route_scoped: bool,
        now_millis: u64,
    ) -> Result<ControlResponse, ControlError> {
        if !route_scoped {
            return Err(ControlError::RouteNotScoped);
        }
        let lease_expires_at_millis = match self.lease.descriptor() {
            None => None,
            Some(_) if self.lease.expired(now_millis) => {
                // A previously paired, authenticated peer must be able to obtain the current
                // descriptor after a lease timeout. The coordinator needs this response to
                // negotiate a fresh generation and send Pair; returning 409 here makes both
                // nodes remain in SoloStandaloneReady forever. Do not establish a lease on a
                // read-only request: the subsequent Pair command still performs the normal
                // authenticated route and generation checks.
                if !self.lease.matches_peer(authenticated) {
                    return Err(ControlError::PeerNotPaired);
                }
                None
            }
            Some(_) => {
                self.lease
                    .renew(authenticated, self.local.generation, true, now_millis)?;
                self.lease.expires_at_millis()
            }
        };
        Ok(ControlResponse {
            status: ControlResponseStatus::Applied,
            generation: self.local.generation,
            descriptor: self.local.clone(),
            lease_expires_at_millis,
        })
    }

    pub(crate) fn handle_validated<F>(
        &mut self,
        endpoint: ControlEndpoint,
        message: ControlMessage,
        authenticated: &AuthenticatedPeer,
        route_scoped: bool,
        now_millis: u64,
        validate: F,
    ) -> Result<ControlResponse, ControlError>
    where
        F: FnOnce(&ControlCommand) -> Result<(), ControlError>,
    {
        if endpoint != message.command.endpoint() {
            return Err(ControlError::EndpointMismatch);
        }
        if message.request_id.is_empty() || message.request_id.len() > 128 {
            return Err(ControlError::InvalidRequestId);
        }
        if matches!(message.command, ControlCommand::Pair { .. })
            && message.generation > self.local.generation
        {
            self.advance_generation(message.generation);
        }
        if message.generation != self.local.generation {
            return Err(ControlError::GenerationMismatch {
                expected: self.local.generation,
                received: message.generation,
            });
        }
        if message.command.requires_matching_deployment()
            && (self.local.deployment_id.is_none()
                || message.deployment_id != self.local.deployment_id)
        {
            return Err(ControlError::DeploymentMismatch);
        }

        let key = (message.generation, message.request_id.clone());
        if let Some(previous) = self.processed.get(&key) {
            if previous != &message {
                return Err(ControlError::IdempotencyConflict);
            }
            self.lease
                .renew(authenticated, message.generation, route_scoped, now_millis)?;
            return Ok(self.response(ControlResponseStatus::Duplicate));
        }

        validate(&message.command)?;

        match &message.command {
            ControlCommand::Pair { descriptor } => {
                if descriptor.protocol_version != 1 || descriptor.role != self.expected_peer_role {
                    return Err(ControlError::InvalidDescriptor);
                }
                self.lease.establish(
                    authenticated,
                    descriptor.clone(),
                    message.generation,
                    route_scoped,
                    now_millis,
                )?;
            }
            _ => self
                .lease
                .renew(authenticated, message.generation, route_scoped, now_millis)?,
        }
        self.processed.insert(key, message);
        Ok(self.response(ControlResponseStatus::Applied))
    }

    fn response(&self, status: ControlResponseStatus) -> ControlResponse {
        ControlResponse {
            status,
            generation: self.local.generation,
            descriptor: self.local.clone(),
            lease_expires_at_millis: self.lease.expires_at_millis(),
        }
    }

    pub(crate) fn lease(&self) -> &PeerLease {
        &self.lease
    }

    pub(crate) fn lease_mut(&mut self) -> &mut PeerLease {
        &mut self.lease
    }

    pub(crate) fn advance_generation(&mut self, generation: u64) {
        self.local.generation = generation;
        self.lease.advance_generation(generation);
        self.processed.clear();
    }

    pub(crate) fn message(&self, request_id: String, command: ControlCommand) -> ControlMessage {
        ControlMessage {
            request_id,
            generation: self.local.generation,
            deployment_id: self.local.deployment_id.clone(),
            command,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.local.generation
    }

    pub(crate) fn local_descriptor(&self) -> &NodeDescriptor {
        &self.local
    }

    /// Compute the session candidate generation for a Pair offer/confirm: the larger of the
    /// local control session generation and the peer's reported generation. Never lower than
    /// either side's known value (design §4). The result cannot exceed `u64::MAX`, but the
    /// computation is explicit so `u64::MAX` is never treated as an overflow.
    pub(crate) fn candidate_generation(&self, peer_generation: u64) -> Result<u64, ControlError> {
        Ok(if peer_generation > self.local.generation {
            peer_generation
        } else {
            self.local.generation
        })
    }
}

fn duration_millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

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
    DistributedReady,
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
            ("POST", "/v1/distributed-ready") => Some(Self::DistributedReady),
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
    fn candidate_generation_never_lowers_the_known_control_session_generation() {
        // Table-driven (G-02, design §4): the candidate for a Pair offer/confirm is the larger
        // of the local control session generation and the peer's reported generation, so a
        // higher worker generation is adopted direction-independently and the candidate never
        // overflows.
        fn processor_at(generation: u64) -> ControlProcessor {
            ControlProcessor::new(
                NodeDescriptor {
                    protocol_version: 1,
                    node_id: "coordinator".into(),
                    role: ControlRole::Coordinator,
                    generation,
                    mode: ControlMode::SoloStandalone,
                    deployment_id: Some("deployment-a".into()),
                },
                ControlRole::Worker,
                Duration::from_secs(15),
                Duration::from_secs(5),
            )
        }
        let cases: [(u64, u64, u64, &str); 5] = [
            (7u64, 7u64, 7, "equal generations keep the local value"),
            (7, 102, 102, "higher peer generation is adopted"),
            (102, 7, 102, "higher local generation is kept"),
            (
                u64::MAX,
                u64::MAX,
                u64::MAX,
                "u64::MAX is preserved without overflow",
            ),
            (0, u64::MAX, u64::MAX, "peer at u64::MAX is adopted"),
        ];
        for (local, peer, expected, label) in cases {
            let processor = processor_at(local);
            let candidate = processor.candidate_generation(peer).expect(label);
            assert_eq!(candidate, expected, "{label}");
            assert!(
                candidate >= local && candidate >= peer,
                "{label}: candidate must not be lower than either known generation"
            );
        }
    }

    #[test]
    fn expired_authenticated_peer_can_read_descriptor_to_restart_pairing() {
        let mut processor = ControlProcessor::new(
            NodeDescriptor {
                protocol_version: 1,
                node_id: "coordinator".into(),
                role: ControlRole::Coordinator,
                generation: 7,
                mode: ControlMode::SoloStandalone,
                deployment_id: Some("deployment-a".into()),
            },
            ControlRole::Worker,
            Duration::from_millis(10),
            Duration::from_millis(1),
        );
        let authenticated =
            AuthenticatedPeer::new_for_test("worker", "10.99.0.2".parse().unwrap(), 100);
        let pair = ControlMessage {
            request_id: "pair-1".into(),
            generation: 7,
            deployment_id: None,
            command: ControlCommand::Pair {
                descriptor: NodeDescriptor {
                    protocol_version: 1,
                    node_id: "worker".into(),
                    role: ControlRole::Worker,
                    generation: 7,
                    mode: ControlMode::SoloStandalone,
                    deployment_id: Some("deployment-a".into()),
                },
            },
        };
        processor
            .handle_validated(
                ControlEndpoint::Pair,
                pair,
                &authenticated,
                true,
                100,
                |_| Ok(()),
            )
            .unwrap();

        let response = processor
            .descriptor_response(&authenticated, true, 200)
            .unwrap();
        assert_eq!(response.generation, 7);
        assert_eq!(response.lease_expires_at_millis, None);

        let wrong_peer =
            AuthenticatedPeer::new_for_test("other-worker", "10.99.0.3".parse().unwrap(), 200);
        assert_eq!(
            processor.descriptor_response(&wrong_peer, true, 200),
            Err(ControlError::PeerNotPaired)
        );
    }

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

    #[test]
    fn reads_the_legacy_distributed_mxfp4_control_mode_alias() {
        let mode: ControlMode = serde_json::from_str("\"distributed-mxfp4\"").unwrap();
        assert_eq!(mode, ControlMode::DistributedLayerParallel);
        assert_eq!(
            serde_json::to_string(&mode).unwrap(),
            "\"distributed-layer-parallel\""
        );
    }
}
