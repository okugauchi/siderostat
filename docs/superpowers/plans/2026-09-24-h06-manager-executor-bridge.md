# H06 Manager Executor Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** runtime の `/manager/jobs` を実行器へ接続し、jobをrunningのまま残さず、domain処理・入力不足・cancelの結果をsucceeded/failedへ正しく終端させる。

**Architecture:** `JobJournal` を共有所有する `ManagerExecutorHandle` を `AppState` に追加する。submit handlerはjournal登録後にbounded queueへ投入し、workerはbackendを実行してjournalへ一度だけterminal更新する。backendは既存manager domain関数を呼ぶ境界とし、fixture backendをテストへ注入できるようにする。

**Tech Stack:** Rust 2024、Tokio mpsc/task、`Arc<Mutex<JobJournal>>`、`AtomicBool` cancellation、Axum admin routes、既存manager domain modules。

**Spec:** `docs/superpowers/specs/2026-09-24-h06-manager-executor-bridge-design.md`

## Global Constraints

- `/manager/*` の認証、公開DTO、HTTP status、payload keyのidempotency契約を維持する。
- `fetch/build/download/verify/stage/activate/rollback` は同じexecutor境界で扱う。
- managerはchildを直接signalせず、activation/rollbackは既存runtime境界へ委譲する。
- 未設定の実機入力・artifact・generation・leaseをfixture成功へ置き換えない。
- cancel後のlate success、secret/raw build logの公開、既存runtime/model/stateの削除・上書きを許可しない。
- 実機受入でruntime/model/stateを変更する操作は、既存のユーザー承認範囲と30分停止時間制約の内側で実施する。

## Review Focus

- queue投入後にworkerが停止した場合もjobがterminalになること — Task 2のqueue failure test。
- cancelとbackend成功が競合した場合に成功へ戻らないこと — Task 2のlate success test。
- 同じpayload keyの重複submitがexecutorで二重実行されないこと — Task 1/4のidempotency test。
- backendエラーにURL認証情報・bearer・raw logが残らないこと — Task 2のredaction test。
- activation/rollbackがgeneration・lease欠落時にdomain実行へ到達しないこと — Task 3/4のcontext test。

---

### Task 1: 共有JobJournalとterminal更新の境界

**Files:**
- Modify: `src/manager/jobs.rs`
- Modify: `src/app.rs:65-160`
- Test: `src/manager/jobs.rs` module tests

**Interfaces:**
- Consumes: existing `JobJournal`, `ManagerJob`, `JobPhase`.
- Produces: `Arc<Mutex<JobJournal>>` ownership in `AppState`; guarded methods `is_cancelling`, `succeed_if_running`, and `fail_if_running`.

- [x] **Step 1: Write failing tests for terminal guards**

```rust
#[test]
fn terminal_guard_rejects_late_success_after_cancel() {
    let mut journal = JobJournal::new();
    let id = journal.enqueue(JobKind::Build, "build-key").expect("enqueue");
    journal.request_cancel(&id).expect("cancel");
    assert!(!journal.succeed_if_running(&id).expect("lookup"));
    assert_eq!(journal.get(&id).expect("job").phase, JobPhase::Cancelling);
}
```

- [x] **Step 2: Run the focused test and confirm RED**

Run: `cargo test --lib manager::jobs terminal_guard_rejects_late_success_after_cancel --features test-support`

Expected: FAIL because the guarded journal methods do not exist.

- [x] **Step 3: Implement guarded journal transitions**

Add methods that inspect the current phase before mutating:

```rust
pub fn is_cancelling(&self, id: &str) -> Result<bool, ManagerJobError>;
pub fn succeed_if_running(&mut self, id: &str) -> Result<bool, ManagerJobError>;
pub fn fail_if_running(&mut self, id: &str, error: impl Into<String>) -> Result<bool, ManagerJobError>;
```

`succeed_if_running` returns `false` for `Cancelling`, `Succeeded`, or `Failed`; `fail_if_running` may close `Running` or `Cancelling` but never overwrites a terminal phase. Move `AppState.jobs` to `Arc<Mutex<JobJournal>>` so the HTTP handlers and executor share one journal.

- [x] **Step 4: Run focused tests and the API regression suite**

Run: `cargo test --test v040_manager_api --features test-support`

Expected: existing six API tests pass; the terminal guard is covered by the focused library test.

- [x] **Step 5: Commit**

```bash
git add src/manager/jobs.rs src/app.rs
git commit -m "Add guarded manager job terminal transitions"
```

### Task 2: Executor queue、cancel registry、fixture backend

**Files:**
- Create: `src/manager/executor.rs`
- Modify: `src/manager/mod.rs`
- Test: `src/manager/executor.rs` module tests

**Interfaces:**
- Consumes: `Arc<Mutex<JobJournal>>` and Task 1 guarded transitions.
- Produces:
  - `ManagerExecutionRequest { id, kind, payload_key, expected_generation, runtime_lease }`
  - `ManagerExecutionOutcome { progress: u8 }`
  - `ManagerExecutionError { Canceled, InputRejected(String), Domain(String), Unavailable }`
  - `trait ManagerExecutionBackend: Send + Sync + 'static { fn execute(&self, request: ManagerExecutionRequest, cancel: Arc<AtomicBool>) -> Result<ManagerExecutionOutcome, ManagerExecutionError>; }`
  - `ManagerExecutorHandle::submit`, `ManagerExecutorHandle::cancel`, `ManagerExecutorHandle::shutdown_for_test`, and `ManagerExecutor::start`.

- [ ] **Step 1: Write failing executor tests**

Add tests for a fixture backend that returns each outcome:

```rust
#[tokio::test]
async fn backend_success_reaches_succeeded() {
    let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
    let request = fixture_request("build-0", JobKind::Build, "fixture-build");
    handle.submit(request).expect("queue");
    worker.await.expect("worker");
    assert_eq!(journal.lock().unwrap().get("build-0").unwrap().phase, JobPhase::Succeeded);
}

#[tokio::test]
async fn backend_error_reaches_failed_without_secret() {
    let (handle, worker, journal) = test_executor(FixtureOutcome::Error(
        "https://user:secret@example.invalid/?token=secret".into(),
    ));
    handle.submit(fixture_request("verify-0", JobKind::Verify, "fixture-verify")).expect("queue");
    worker.await.expect("worker");
    let job = journal.lock().unwrap().get("verify-0").unwrap().clone();
    assert_eq!(job.phase, JobPhase::Failed);
    assert!(!job.error.contains("secret"));
}

#[tokio::test]
async fn cancel_wins_over_late_backend_success() {
    let (handle, worker, journal) = test_executor(FixtureOutcome::SuccessAfterCancel);
    handle.submit(fixture_request("download-0", JobKind::Download, "fixture-download")).expect("queue");
    handle.cancel("download-0").expect("cancel");
    worker.await.expect("worker");
    assert_ne!(journal.lock().unwrap().get("download-0").unwrap().phase, JobPhase::Succeeded);
}

#[tokio::test]
async fn closed_queue_marks_submitter_error() {
    let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
    handle.shutdown_for_test();
    let result = handle.submit(fixture_request("stage-0", JobKind::Stage, "fixture-stage"));
    assert!(matches!(result, Err(ManagerExecutorError::QueueClosed)));
    worker.await.expect("worker");
    assert!(journal.lock().unwrap().get("stage-0").is_none());
}
```

- [ ] **Step 2: Run executor tests and confirm RED**

Run: `cargo test --lib manager::executor`

Expected: FAIL because the executor module and backend traits are absent.

- [ ] **Step 3: Implement the bounded executor**

Use `tokio::sync::mpsc::channel(16)` and a `HashMap<String, Arc<AtomicBool>>` protected by `Mutex` for cancellation. The worker must call the synchronous backend inside `tokio::task::spawn_blocking`, then apply exactly one guarded journal transition. On `Canceled`, `QueueClosed`, backend error, or panic conversion, call `fail_if_running`; never call `succeed_if_running` when the cancel flag is set.

Redact backend errors before writing them to the journal by removing URL userinfo, bearer/token query values, and `raw build log` labels. Install a manager-owned thread-aware panic dispatcher with direct `set_hook` before worker execution and before each backend call; it emits a fixed non-secret message for unrelated panics, suppresses payload for the backend thread marker, never calls `take_hook`, and converts backend panics to `Unavailable`. Validate that the queued request's `kind` and `payload_key` match the journal entry before executing it. Update `JobJournal::enqueue`'s running index to use the `(kind, payload_key)` pair so same keys for different kinds cannot share an ID.

- [ ] **Step 4: Run executor tests and verify GREEN**

Run: `cargo test --lib manager::executor`

Expected: all executor lifecycle, cancellation, queue, and redaction tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/manager/executor.rs src/manager/mod.rs
git commit -m "Add manager executor lifecycle bridge"
```

### Task 3: Runtime input resolver and safe backend adapter

**Files:**
- Modify: `src/manager/executor.rs`
- Modify: `src/manager/api.rs`
- Test: `tests/v040_manager_executor.rs` (new)

**Interfaces:**
- Consumes: `ManagerExecutionBackend`, existing manager domain functions, `JobSubmitRequest` context.
- Produces: `RuntimeManagerBackend` and `ManagerJobInputResolver` with explicit typed rejection for unavailable real inputs.

- [ ] **Step 1: Write failing resolver and backend tests**

Create fixture inputs for all seven kinds. Assert that valid fixture plans call the corresponding domain adapter and finish, while these cases fail before domain execution:

```rust
#[test]
fn unknown_payload_key_is_rejected_without_domain_call() {
    let backend = fixture_backend_with_call_counter();
    let error = backend.execute(
        fixture_request("fetch-0", JobKind::Fetch, "unknown"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(error, Err(ManagerExecutionError::InputRejected(_))));
    assert_eq!(backend.domain_call_count(), 0);
}

#[test]
fn activation_without_generation_or_lease_is_rejected() {
    let backend = fixture_backend_with_call_counter();
    let mut request = fixture_request("activate-0", JobKind::Activate, "fixture-activate");
    request.expected_generation = 0;
    request.runtime_lease = None;
    let error = backend.execute(request, Arc::new(AtomicBool::new(false)));
    assert!(matches!(error, Err(ManagerExecutionError::InputRejected(_))));
    assert_eq!(backend.domain_call_count(), 0);
}

#[test]
fn unavailable_real_model_is_terminal_failure_not_pending_forever() {
    let backend = RuntimeManagerBackend::without_model_catalog();
    let request = fixture_request("download-0", JobKind::Download, "mxfp4-0731");
    let error = backend.execute(request, Arc::new(AtomicBool::new(false)));
    assert!(matches!(error, Err(ManagerExecutionError::Unavailable)));
}
```

- [ ] **Step 2: Run the new integration test and confirm RED**

Run: `cargo test --test v040_manager_executor --features test-support`

Expected: FAIL because the resolver/backend adapter is absent.

- [ ] **Step 3: Implement typed input resolution**

Define `ManagerJobInputResolver` with one method per `JobKind` or a typed enum return. The runtime resolver accepts only configured payload keys and returns `InputUnavailable` when a required source checkout, catalog checksum, verified artifact, production runtime, or previous digest is absent. It must pass generation/lease through unchanged for activate/rollback and reject zero/empty context before invoking any domain function.

Add `FixtureManagerBackend` for tests. Add `RuntimeManagerBackend` for production; it invokes existing pure/domain functions when a complete resolver plan exists and returns `Unavailable` or `InputRejected` for missing real inputs. It must never report success merely because a journal entry was created.

- [ ] **Step 4: Run the resolver integration tests**

Run: `cargo test --test v040_manager_executor --features test-support`

Expected: seven fixture kinds reach terminal success, negative cases reach redacted failed, and no real network/process is used.

- [ ] **Step 5: Commit**

```bash
git add src/manager/executor.rs src/manager/api.rs tests/v040_manager_executor.rs
git commit -m "Connect manager executor to typed runtime inputs"
```

### Task 4: AppState/API wiring and monitor contract

**Files:**
- Modify: `src/app.rs:65-160,2265-2335`
- Modify: `src/manager/api.rs`
- Modify: `monitor/src/manager_window.rs` only where terminal/error display needs a contract correction
- Test: `tests/v040_manager_api.rs`, `monitor/tests/v040_monitor_contract.rs`

**Interfaces:**
- Consumes: `ManagerExecutorHandle`, `RuntimeManagerBackend`, guarded `Arc<Mutex<JobJournal>>`.
- Produces: submit/cancel routes that enqueue and cancel the same job id; status polling observes terminal states.

- [ ] **Step 1: Write failing route lifecycle tests**

Extend the existing Axum test helpers to submit a fixture-backed job, poll `/manager/jobs/{id}` until `succeeded` or `failed`, then submit a cancelled job and assert it never becomes `succeeded`.

- [ ] **Step 2: Run the route tests and confirm RED**

Run: `cargo test --test v040_manager_api --features test-support`

Expected: a submitted manager job remains `running` because the routes are not yet wired to the executor.

- [ ] **Step 3: Wire executor ownership into AppState**

Construct `Arc<Mutex<JobJournal>>` and `ManagerExecutorHandle` together in `AppState::from_config`. Add `AppState::from_config_with_manager_backend` for test state construction so integration tests can inject `FixtureManagerBackend`; production construction uses `RuntimeManagerBackend`. `manager_jobs_submit` must retain the existing `202` response, enqueue the returned job id, and fail the journal entry if queue submission fails. `manager_job_cancel` must call both `api::cancel` and `executor.cancel` before returning `200`.

Use the fixture backend only in test state construction; production state uses `RuntimeManagerBackend`. Do not start a second HTTP server or AppKit event loop.

- [ ] **Step 4: Run API, monitor, and root manager suites**

Run:

```bash
cargo test --test v040_manager_api --features test-support
cargo test --manifest-path monitor/Cargo.toml --test v040_monitor_contract --features test-support
cargo test --all-targets -- --skip w09_all_fixtures_accepted_by_real_codex_parser
```

Expected: submitted jobs reach honest terminal states; existing monitor and root suites remain green.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/manager/api.rs monitor/src/manager_window.rs tests/v040_manager_api.rs monitor/tests/v040_monitor_contract.rs
git commit -m "Wire manager executor into runtime API"
```

### Task 5: Verification gates and H06 evidence update

**Files:**
- Modify: `docs/superpowers/plans/2026-09-24-h06-manager-executor-bridge.md`
- Modify: `Vault: /Users/o/LLM/obsidian-vault/Projects/siderostat/v0.4.0-implementation/evidence/H06.md`

- [ ] **Step 1: Run all repository gates**

Run:

```bash
cargo test --all-targets -- --skip w09_all_fixtures_accepted_by_real_codex_parser
cargo test --manifest-path monitor/Cargo.toml --features test-support --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Expected: all executable tests, monitor tests, format, clippy, and diff checks pass. The skipped root test remains documented as an external missing `codex-replay` binary and is not reclassified as a product failure.

- [ ] **Step 2: Build and inspect app-dev without touching official runtime**

Run: `cargo xtask app-dev --version 0.4.0 --build-number 3 --verify`

Expected: bundle version/build and ad-hoc signature verification pass; no installed runtime/model/state is stopped or modified.

- [ ] **Step 3: Update the plan and Vault evidence**

Record the executor commits, route lifecycle results, fixture matrix, skipped external test, and the remaining physical H06 cases. Keep H06 `PENDING` until GUI clean-root, failed-build recovery, startup rollback, upgrade restore, and profile minimal-request cases are actually observed.

- [ ] **Step 4: Commit the verification record**

```bash
git add docs/superpowers/plans/2026-09-24-h06-manager-executor-bridge.md
git commit -m "Record H06 executor bridge verification"
```

## Execution Notes

- The implementation is deliberately limited to the executor bridge and its honest terminal-state behavior. It does not silently turn missing model/catalog/runtime prerequisites into a GUI success.
- The existing accepted StackView layout remains unchanged; later design work can be isolated in a separate commit.
- Remote push and physical H06 acceptance remain separate operations and require an explicit follow-up after verification.
