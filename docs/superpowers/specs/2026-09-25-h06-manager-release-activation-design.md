# H06 Manager Release, Activation, and Rollback Design

## Purpose

Complete the H06 Manager workflow so an operator can prepare a DS4 release on each node, verify its artifacts, stage a profile, and explicitly activate or roll back that profile through the existing runtime lifecycle owner. A job is successful only after its domain result and durable record are published. An activation is successful only after both nodes have committed and acknowledged the same transaction.

This design extends the approved Manager Executor Bridge. It does not treat job submission, configured manifests, or a single node's readiness as proof that an operation succeeded.

## Approved scope and decisions

The user approved the following scope and boundaries during design review on 2026-09-25:

- Cover Fetch, Build, Download, Verify, Stage, Activate, and Rollback as one end-to-end Manager capability.
- Keep `ManagerExecutor` responsible for domain jobs and introduce a runtime-owned coordinator/actor for activation and rollback. The executor must never signal or start an owned DS4 child directly.
- Persist source receipts, build/model artifacts, profiles, jobs, and activation transactions under the existing managed root. Retain the previous active release for rollback; do not automatically delete artifacts.
- Each node prepares its own artifacts through that node's Manager GUI. Peer control exchanges typed identifiers, checksums, and transaction acknowledgements; it does not transfer large model or binary files.
- Require both nodes to prepare before draining. Keep routes closed until both nodes are ready and commit acknowledgements are confirmed. Ambiguous recovery remains fail-closed and requires reconciliation or manual intervention.
- Use an app-bundled fixed model catalog for the initial scope. Do not accept arbitrary URLs, paths, shell commands, or catalog entries from the GUI.
- Keep Siderostat self-update and automatic artifact cleanup out of scope.

## Existing constraints

- `src/manager/executor.rs` currently discards `SourceRecord` and `BuildOutcome`, has no production plans, and returns unavailable for Activate/Rollback.
- `ArtifactRegistry` records are held in memory and can be journaled, but there is no strict production journal-load path. The source receipt has no durable store.
- `ProductionClusterRuntime` owns fixed child lifecycles created from startup configuration. The production activation module currently provides a state machine and fakeable driver, but no runtime-backed activation driver.
- The peer control router is authenticated and supports lifecycle and policy operations, but it has no artifact transfer protocol.
- `AppState` starts the Manager executor before the production runtime and supervisor are attached. The runtime command boundary therefore needs a channel that can be attached before listeners begin serving.

## Components and ownership

### Per-node Manager release store

Add a versioned, private store under `ManagerRoot` for immutable source receipts, artifact records, profile records, active/previous references, job history, and activation transaction journals. Keep large data in separate immutable files; metadata updates use temporary files, file and directory sync, and atomic rename. Recovery validates schema, full hashes, relative paths, symlink containment, node identity, and record references before exposing inventory.

Publish artifacts in this order: write to a private temporary path; compute and compare the full SHA-256; sync the file; atomically rename to a content-addressed managed path; then atomically publish the metadata record. A crash before metadata publication can leave an unreferenced file, but cannot create a trusted registry record. Existing user files and the configured active runtime are not copied, deleted, or rewritten.

### Manager executor

Each node runs its own executor for Fetch, Build, Download, Verify, and Stage. Inputs resolve to typed, allowlisted records:

- Fetch uses the canonical pinned DS4 remote and `main` reference. The UI cannot choose a remote or arbitrary revision. A successful Fetch persists the full commit, main-ancestry proof, remote identity, and fetch time before succeeding.
- Build selects a persisted source receipt and an allowlisted local role. It creates an isolated checkout pinned to the receipt's full commit, checks exact HEAD and a clean worktree, runs the existing fixed target through the owned process-group runner, verifies the output, and publishes an immutable build artifact plus `BuildRecord`.
- Download accepts only an ID from the app-bundled catalog. It writes into a managed temporary path and cannot publish an artifact until size and full SHA-256 match.
- Verify rechecks the actual bytes against the trusted catalog or build record and persists the verified state.
- Stage creates a profile record from verified node-local role artifacts, the verified model artifact, and the validated current runtime configuration. It does not start or stop a child.

Build workspaces are unique per job and are not treated as artifact records. Published artifacts are immutable and addressed by server-generated IDs and full digests. Every write and checkout is constrained to the managed namespace. Cancellation stops and reaps the owned Git or build process group before the job becomes terminal.

### Runtime-owned coordinator

Create one asynchronous coordinator per runtime node, owned by `ProductionClusterRuntime`. `AppState` provides a command channel to it before the admin listener starts; attachment completes after runtime construction. It is the only layer allowed to coordinate child lifecycle changes, and it routes actual start/stop work through the existing supervisor owners.

The coordinator serializes Manager activation/rollback against policy changes, planned restart, pairing/promotion, and recovery. It reads live runtime state at execution time, validates `expected_generation`, obtains and validates the current runtime/peer lease internally, and checks the latest policy epoch. The public Manager API does not accept or expose a runtime lease. If the active policy forbids the requested distributed activation, the job yields before draining. If policy changes after draining begins, the transaction restores the previous profile and preserves the newer policy epoch.

Supervisors gain a controlled way to use a new validated `Ds4Command` on the next start. The runtime owner switches the command only while it holds the activation lifecycle gate. Neither Manager executor code nor an HTTP handler may obtain a child handle or signal a child.

## Durable data model

Store strict, versioned records for:

- **Source receipt:** canonical remote identity, full commit, proof that the commit is an ancestor of the pinned main ref, fetch time, and schema version.
- **Artifact:** generated ID, kind, relative managed path, full SHA-256, size, validation state, and provenance. Build artifacts also retain source commit, role, target, flags, toolchain, architecture, and help digest. Model artifacts retain catalog ID and compatibility metadata.
- **Staged profile:** profile identity, node role, references to verified artifacts, model catalog identity, validated runtime settings, compatibility result, and hardware-readiness state. It cannot contain arbitrary argv or unmanaged file paths.
- **Release references:** each node's current and previous release identities and their artifact digests. The original user-configured runtime is recorded as an external baseline by a fingerprint of the validated config and observed executable/model digests; raw command arguments are not copied into the journal. Rollback rebuilds the command from the current validated config and proceeds only if the config fingerprint and required file digests still match. The baseline is not copied or automatically removed.
- **Job journal:** request identity, kind, typed input IDs, timestamps, progress, and terminal outcome. No credentials, raw build logs, bearer tokens, or runtime leases are persisted.
- **Activation journal:** operation ID, expected generation, policy epoch, per-node phases, per-node candidate and previous digests, and sanitized failure class. Raw runtime leases are not persisted.

All readers reject unknown schema versions and invalid references. Upgrade never silently resets an unreadable registry. A running/cancelling job discovered after process restart becomes `interrupted`; it is never inferred to have succeeded. Startup loads activation journals before starting any DS4 child. If every node has durable commit acknowledgement and matching active pointers, recovery may close the transaction after live identity checks. Otherwise recovery restores the previous release on both nodes before child startup. If either outcome cannot be proven or previous cannot be restored, do not start a candidate child and keep admission closed in manual intervention.

## Manager API and GUI

Retain bearer authentication and the existing job status/cancel semantics. Add a sanitized authenticated inventory endpoint that exposes source commits, build/model artifact IDs, staged profiles, per-node readiness, active/previous digest, and activation phase without raw paths or secrets. The configured manifest is shown as configuration intent; the active digest is populated only from a live runtime observation bound to a verified release record.

The job API continues to accept typed operation identifiers rather than remote URLs, paths, or command strings. Fetch has a fixed key. Build references a source receipt and allowlisted role. Download references a bundled catalog entry. Verify and Stage reference local artifact/profile IDs. Activate references a staged profile and expected generation. Rollback references the previous release. The runtime coordinator supplies the live lease internally.

Preparation is node-local: operators use each node's GUI to run its own Fetch/Build/Download/Verify/Stage jobs. Inventory shows each node's own prepared state and the peer's sanitized readiness summary. Activation and Rollback may be initiated from the local GUI; the coordinator-role node owns the cluster transaction and forwards typed requests over the authenticated peer control channel. A request initiated on the worker is forwarded idempotently to the coordinator. The GUI shows per-node job and activation phases, redacted failure reasons, active/previous digest, and keeps Activate disabled until all preconditions are satisfied. Activation and rollback always require an explicit operator action; no pipeline job auto-activates a profile.

The initial bundled catalog is the only source of model URLs and trusted sizes, SHA-256 values, licenses, and compatibility metadata. It is shipped as a resource covered by the app's code signature; updating that catalog requires a Siderostat app update. The public Manager API never returns filesystem paths or catalog credentials.

## Activation and rollback transaction

For a cluster-enabled runtime, the coordinator node owns the global journal and protocol. The local and peer runtime coordinators own their respective local child lifecycles.

1. **Preflight:** persist the operation intent. Verify the generation, current policy epoch, live lease, both node identities, each node's local staged profile, required role artifact, shared model digest, and availability of a valid previous release. Rehash managed files immediately before use. Missing or mismatched artifacts fail before any child stops.
2. **Prepare:** each node records its candidate and previous digests and returns a durable prepare acknowledgement. The coordinator proceeds only after both acknowledgements identify the same operation and compatible profile.
3. **Drain:** close admission and perform the existing runtime-owned drain/stop sequence on both nodes. Persist each node phase. If either node cannot drain safely, restore any already-stopped node and abort.
4. **Start and readiness:** install the candidate command under the lifecycle gate, start each node's owned child, and wait for its configured readiness checks. The route remains closed while either node is not ready. A failure starts the previous command on both nodes.
5. **Commit:** each node atomically records its local active pointer only after its candidate child is ready, then returns a commit acknowledgement. The cluster transaction becomes `Complete` and serving routes reopen only after both acknowledgements and a final generation/policy/lease check. A lost acknowledgement remains `Committing`; it never becomes Complete by assumption.
6. **Rollback:** on candidate failure, explicitly or automatically restore the recorded previous command on both nodes and wait for readiness. Preserve the latest policy epoch and never delete the failed candidate. If either previous child fails to start, keep admission closed and report manual intervention.

The local single-node mode uses the same journal and lifecycle owner with one participant. A cluster-enabled operation with a missing or unreachable peer fails before drain. The Force/standalone latch is not cleared by Manager work. An operation that would violate the current policy is yielded; a rollback already required to recover a partially applied transaction may restore the previous state while retaining the latest policy epoch.

The transaction protocol is not a distributed filesystem atomic write. Safety comes from durable per-node journals, idempotent operation IDs, route gating, per-node acknowledgements, and reconciliation before serving after restart.

## Failure behavior and security

- Domain errors are mapped to fixed, redacted public classes. Do not log raw Git paths, URL credentials/query values, catalog credentials, panic payloads, or raw build output.
- Queue failure, cancellation, child failure, or process restart closes the relevant job terminally. A terminal job is not rewritten by a late worker result.
- Cancellation of Git/make kills and reaps only the owned process group. Cancellation does not delete published artifacts or mutate active state.
- A short checksum, unknown artifact, changed file, escaping path, stale generation/lease, missing peer, policy mismatch, one-sided readiness, or missing acknowledgement cannot produce Ready/Complete.
- Neither ordinary job execution nor failed activation deletes the previous artifact, external baseline, model, user configuration, or cluster policy.

## Acceptance and verification

### Automated tests

- Store: atomic write/reopen, schema rejection, source receipt validation, full-hash verification, symlink/path escape rejection, interrupted job handling, and crash points before/after artifact and active-pointer publication.
- Pipeline: local bare fixture remote fetches to a persisted full commit; a process-reopened store resolves the same receipt; a job-specific clean checkout builds the requested role; output digest and BuildRecord persist; cancellation reaps Git and make helpers; failure never changes active references.
- Catalog/model path: only bundled IDs resolve; missing/short/wrong digest fails; successful download and verify survive process restart; stage rejects incompatible role/model/config and never starts a child.
- Runtime actor: stale generation/lease/policy rejection; operation serialization with policy/recovery/planned restart; no direct child signal outside the supervisor owner; local one-node activation; two-node prepare/drain/start/commit; duplicate operation idempotency; crash/restart reconciliation at every transaction phase.
- Fault matrix: either node missing artifact, peer disconnect, one-sided drain/start failure, lost ready/commit acknowledgement, Force policy change, candidate start failure, previous start failure, and partial commit. Assert no false Complete, old active preservation or restored previous, and closed admission/manual intervention when unresolved.
- API/GUI: bearer gate, typed IDs only, inventory redaction, stable terminal job display, per-node readiness, disabled activation until preflight passes, explicit activation/rollback, cancellation, and refresh after runtime/store changes.
- Run root and monitor all-target suites, format check, Clippy with warnings denied, and app-dev bundle verification.

### Physical acceptance

After automated gates pass, use each node's Manager GUI to prepare its artifacts. From a GUI, activate and observe both node phases and the live active digest. Exercise a candidate startup failure and confirm both nodes return to previous; exercise an interrupted transaction/restart and confirm reconciliation keeps admission closed until both nodes agree; then run explicit rollback. H06 remains PENDING until these GUI and runtime observations are recorded.

Any physical run that may stop a live runtime must be estimated against the previously authorized 30-minute outage limit. Ask the user for an extension before proceeding if the estimate exceeds that limit; do not silently continue past it.

## Delivery decomposition

Implement in dependency order, each with its own reviewable change and tests:

1. Durable store, strict load/recovery, job journal, source receipts, and immutable artifact publication.
2. Production Fetch/Build/Download/Verify/Stage inputs and per-node inventory API/GUI.
3. Runtime lifecycle command slots and local single-node activation/rollback actor.
4. Authenticated peer transaction protocol, two-node activation/rollback, and reconciliation.
5. GUI cluster state/controls, full regression gates, app-dev verification, and physical acceptance evidence.

Do not mark H06 complete after code or fixture tests alone. Physical acceptance remains a separate gate.
