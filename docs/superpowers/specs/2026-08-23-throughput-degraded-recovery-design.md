# Throughput Degraded Recovery Design

> Phase H-01 design record. The normative contract is [`docs/recovery/throughput-degraded-contract-v0.3.0.md`](../../recovery/throughput-degraded-contract-v0.3.0.md).

## Goal

Define a bounded, coordinator-owned recovery workflow for inference throughput degradation that can be implemented incrementally without bypassing the existing Siderostat cluster lifecycle or killing unverified processes.

## Scope

This design covers the H-01 contract only: detection semantics, initial thresholds, recovery API shape, ownership/idempotency, recovery sequence, failure states, and redaction boundaries. H-02 through H-10 implement and verify the contract in dependency order.

## Architecture

The recovery service is an orchestration layer above the existing admission gate and production cluster lifecycle. It does not become a second source of truth for cluster state and does not send signals to worker processes. A coordinator-owned recovery job records a bounded history, writes a redacted snapshot before mutation, delegates demotion and promotion to the existing lifecycle owners with an operation-scoped admission-drain deadline, and reopens admission only after a post-recovery canary succeeds. During the recovery admission block, normal external inference requests are not accepted; after drain, the recovery owner may issue one single-use internal canary exception for the fixed post-recovery probe. This is an admission-gate exception, not ds4-server request priority, a reserved slot, or a queue bypass. The normal cluster drain and DS4 stop defaults remain independent at 180 seconds; throughput recovery uses a separate 60-second admission-drain default.

The detector has two independent inputs: monotonic progress freshness for active requests and a bounded canary for an actual inference check. Idle zero TPS is explicitly healthy. Automatic recovery is opt-in and disabled by default until two-node evidence is accepted.

## Alternatives considered

1. **Direct worker restart** — rejected. The distributed pipeline is a pair of children; a worker-only restart can leave control session, generation, or KV/session state inconsistent and would bypass the existing owner.
2. **Reuse `cluster restart` as the recovery operation** — rejected. `cluster restart` preserves the current mode and is not a throughput-specific workflow with snapshot, cooldown, canary, and bounded history.
3. **Coordinator recovery job using existing demote/promote owners** — selected. It reuses already-tested lifecycle transitions, keeps safety checks in one place, and makes the recovery operation observable and idempotent.

## State and data boundaries

- Existing `StableMode`, `ClusterState`, `ProxyTarget`, and `AdmissionState` remain authoritative.
- A recovery job has its own phase and result; its phase is never serialized as cluster state.
- Recovery identity is a UUID plus an idempotency key, but high-cardinality identifiers are excluded from metrics labels.
- Diagnostic snapshots are redacted, atomic, permission-controlled, and bounded-retention artifacts.
- User data, prompt/response content, secrets, session IDs, and full deployment identifiers are outside the schema.

## Failure and safety model

The workflow is fail-closed. It must not mutate admission or cluster state before the snapshot succeeds. The recovery-specific admission-drain timeout does not authorize unconditional request termination or DS4 stop; if no lifecycle mutation occurred and generation/target are unchanged, the temporary admission block is rolled back to serving, otherwise manual intervention is required. A demotion or promotion failure uses the existing safe-state/backoff/manual-intervention semantics and never starts a second restart loop. A failed post-recovery canary does not reopen serving admission.

## Verification strategy

H-01 is verified by document review against Hermes research sections 8.1, 8.2, 9.1, 9.2, 9.4, 9.5, 10, 11, and 12, plus terminology checks against `src/target.rs`, `src/admission.rs`, and `docs/operations.md`. H-02 onward must add deterministic unit or integration tests for each acceptance-matrix row before H-10 real-machine validation.

## Review gate

The operator must review and approve the normative contract before H-02 implementation starts. In particular, review must cover the 60-second `recovery.admission_drain_timeout`, the unchanged 180-second normal cluster/DS4 timeouts, the `enabled=false` default, the 12-hour recovery limit, the timeout rollback behavior, the post-canary serving gate, the recovery canary exception semantics, and the redaction boundary. The operator approved this contract on 2026-08-23, including the admission block for normal external requests and the single-use canary exception without assuming ds4-server request priority, a reserved slot, or queue bypass.
