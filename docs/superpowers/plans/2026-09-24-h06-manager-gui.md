# H06 Manager GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** メニューバーから開く単一のAppKit管理ウィンドウを追加し、既存のManagerViewModel/ModelViewとmanager APIを接続して、H06のGUI gateを実機で再開できる状態を作る。

**Architecture:** AppKit windowはmonitor process内に1つだけ保持し、main threadは表示とcommand/eventの受け渡しだけを担当する。HTTPとjob pollingはworker threadでMetricsClientを使って実行し、既存view modelへイベントを適用する。runtime側のJobJournalがterminal状態へ進まない場合はGUIで成功扱いにせず、H06をpendingとして記録する。

**Tech Stack:** Rust 2024, objc2/objc2-app-kit, tray-icon, reqwest, tokio, existing ManagerViewModel/ModelView and MetricsClient.

**Spec:** `docs/superpowers/specs/2026-09-24-h06-manager-gui-design.md`

## Global Constraints

- 既存runtime、model、secret、manifest、SMAppService登録をGUI受入のために削除・上書きしない。
- AppKit main loopをnetwork I/Oでブロックしない。
- activation/rollbackはgenerationとruntime leaseが不足する場合に送信せず、disabled理由を表示する。
- job errorは`redact_secrets`を通して表示し、token・URL userinfo・query secret・raw build logを露出しない。
- Manager windowは単一hostで再利用し、二つ目のSiderostat processを起動しない。
- terminal状態を観測できないjobを成功扱いせず、H06 evidenceをPASSへ変更しない。
- 変更は`feature/v040-integration`にreview可能な目的単位で記録する。

## Review Focus

- window close/reopen時にjob状態が消えず、二重window・二重menu iconを作らない。
- activation/rollbackでgeneration・lease不足をruntimeへ送らない。
- build/download中に旧active digestを表示し、失敗・cancel後も旧状態を維持する。
- checksum、Vision encoder、support、prefix-file不整合をactivate可能として表示しない。
- manager APIがrunningのままの場合にGUIが誤って成功表示しない。

### Task 1: Manager API context境界を実装する

**Files:**
- Modify: `monitor/src/client.rs`
- Modify: `monitor/src/manager_window.rs`
- Test: `monitor/tests/v040_manager_api.rs`
- Test: `monitor/src/client.rs` unit tests

**Interfaces:**
- Add `MetricsClient::submit_manager_job_with_context(kind, payload_key, expected_generation, runtime_lease)` returning `SubmitResponse`.
- Keep `submit_manager_job` for non-activation jobs and route it through the new request builder with zero/none context.
- Extend the `ManagerApi` test boundary with a context-aware method whose default implementation preserves existing fake behavior.

- [x] **Step 1: Write failing tests**
  - Verify serialized activation/rollback requests contain nonzero `expected_generation` and `runtime_lease`.
  - Verify a missing context is rejected before a mutating request is sent.
  - Verify existing fetch/build/download/verify/stage requests keep their current payload.

- [x] **Step 2: Run the focused tests and confirm RED**
  - Run `cargo test --test v040_manager_api --features test-support`.
  - Expected: the new context assertions fail because the client has no context-aware request path.

- [x] **Step 3: Implement the minimal client/trait changes**
  - Add one internal JSON request builder and one context-aware submit method.
  - Preserve the existing error mapping and bearer authentication.
  - Update the fake adapter only where the new signature requires it.

- [x] **Step 4: Run the focused tests and confirm GREEN**
  - Run `cargo test --test v040_manager_api --features test-support`.
  - Expected: all existing and new cases pass.

### Task 2: Implement the single AppKit manager window host

**Files:**
- Modify: `monitor/src/manager_window.rs`
- Modify: `monitor/src/lib.rs`
- Test: `monitor/tests/v040_manager_window_host.rs`

**Interfaces:**
- Add `ManagerWindowHost::new(mtm, client, initial_state)`.
- Add `ManagerWindowHost::show_or_focus()` and `ManagerWindowHost::is_visible()`.
- Add a command/event channel boundary using explicit `ManagerCommand` and `ManagerEvent` enums.
- Keep `ManagerViewModel` and `ModelView` independent from AppKit objects so existing tests remain pure.

- [x] **Step 1: Write failing host tests**
  - Verify repeated `show_or_focus()` reuses one host/window identity.
  - Verify a close/reopen cycle keeps a running job in the view model.
  - Verify command dispatch emits fetch/build/cancel/activate/rollback commands without executing network I/O on the caller.

- [x] **Step 2: Run the focused host tests and confirm RED**
  - Run `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_window_host --features test-support`.
  - Expected: the host and command types are not yet present.

- [x] **Step 3: Implement the host and controls**
  - Create an AppKit titled window on the main thread with Runtime/source, Artifact pipeline, Profiles, Activation/rollback, and Jobs sections.
  - Retain the window and controls in one host; focus the existing window on repeated open.
  - Use labels and disabled buttons for unsupported/missing-checksum/prefix-file cases.
  - Route control actions to the command channel; do not perform HTTP from button callbacks.
  - Update visible state only from `ManagerEvent` snapshots and redact error text before assigning it to labels.

- [x] **Step 4: Run host tests and compile checks**
  - Run the focused host test and `cargo test --manifest-path monitor/Cargo.toml --features test-support --all-targets`.
  - Expected: host tests and the existing monitor suite pass.

### Task 3: Wire the manager window into the tray and worker loop

**Files:**
- Modify: `monitor/src/tray.rs`
- Modify: `monitor/src/main.rs`
- Modify: `monitor/src/manager_window.rs`
- Test: `monitor/tests/v040_monitor_contract.rs`

**Interfaces:**
- Add a distinct tray menu id for `管理画面を開く` and a matching `MonitorTray::is_open_manager_event` helper.
- Add one manager worker thread that consumes `ManagerCommand`, calls `MetricsClient`, and sends `ManagerEvent` snapshots.
- Add main-loop polling that applies events to the host and updates the window without blocking tray refresh.

- [x] **Step 1: Write failing menu/dispatch tests**
  - Assert the manager menu id is unique and only the manager helper matches it.
  - Assert the worker maps submit errors to redacted failure events and never marks a nonterminal job as succeeded.

- [x] **Step 2: Run focused tests and confirm RED**
  - Run `cargo test --manifest-path monitor/Cargo.toml --test v040_monitor_contract --features test-support`.
  - Expected: the new helper and dispatch behavior are absent.

- [x] **Step 3: Implement the tray and worker wiring**
  - Add the menu item near the existing settings/restart actions.
  - Create the host after `NSApplication`/tray initialization, before `app.run()`.
  - Handle the menu event by calling `show_or_focus()` only.
  - Poll manager events from the existing main-thread timer; apply status to `ManagerViewModel`/`ModelView` and keep active digest visible during build/download.
  - Keep manager jobs independent of `OperationState` so a long build does not disable unrelated tray status updates.

- [x] **Step 4: Run the monitor suite and static gates**
  - Run `cargo test --manifest-path monitor/Cargo.toml --features test-support --all-targets`.
  - Run `cargo fmt --all -- --check` and `cargo clippy --all-targets --all-features -- -D warnings`.
  - Expected: all tests pass and no lint/format errors remain.

### Task 4: Add fixture coverage for H06 negative paths and profile display

**Files:**
- Modify: `monitor/tests/v040_manager_api.rs`
- Modify: `monitor/tests/v040_manager_window_host.rs`
- Create if needed: `monitor/tests/fixtures/v040_manager_gui/*`
- Modify: `monitor/src/manager_window.rs`

**Interfaces:**
- Fixture events represent running, cancelling, succeeded, failed, missing checksum, Vision encoder mismatch, unsupported support model, and prefix-file mismatch.
- Profile rows expose a pending state for artifacts that are not prepared.

- [x] **Step 1: Write failing fixture assertions**
  - Assert redacted errors never expose password/token values.
  - Assert old active digest remains visible while a build/download job is running.
  - Assert each incompatible profile disables activation and shows a reason.
  - Assert an unprepared artifact is `pending`, never `succeeded`.

- [x] **Step 2: Run fixture tests and confirm RED**
  - Run `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_window_host --features test-support`.
  - Expected: new fixture assertions fail before the UI state projection is complete.

- [x] **Step 3: Implement the projection and fixture adapters**
  - Reuse `ManagerViewModel::redacted_reason`, `ModelView::vision_reason`, and `ModelView::can_activate`.
  - Do not add a second compatibility implementation in the AppKit layer.

- [x] **Step 4: Run all focused tests**
  - Run `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_window_host --features test-support`.
  - Expected: all fixture cases pass.

### Task 5: Build the app-dev bundle and perform GUI smoke only

**Files:**
- Modify: `Vault: /Users/o/LLM/obsidian-vault/Projects/siderostat/v0.4.0-implementation/evidence/H06.md`
- Do not modify installed runtime/model/state.

- [x] **Step 1: Build a fresh app-dev bundle from the feature branch**
  - Use the repository's existing macOS build/install script, outputting to `build/app-dev`.
  - Verify bundle version/build and that the app contains the new manager menu item.

- [x] **Step 2: Ensure only the official app process is running**
  - Confirm `/Applications/Siderostat.app` is the only Siderostat process before the smoke.
  - Do not stop `siderostat-runtime`; verify `/healthz` and `/readyz` remain unchanged.

- [x] **Step 3: Run GUI smoke with a small fixture**
  - Open the manager window from the official menu bar icon.
  - Confirm one window only, close/reopen job state retention, redacted failure display, and pending profile display.
  - Do not run destructive upgrade/rollback until the runtime executor terminal-state behavior is observed.

- [x] **Step 4: Record evidence without overstating H06**
  - Record command, bundle identity, process identity, case result, and runtime health in `evidence/H06.md`.
  - Keep H06 `PLANNED`/`PENDING` if manager jobs remain running or the GUI gate is incomplete.

### Task 6: Verify branch state and prepare review

**Files:**
- No runtime/model files.
- Update: `docs/superpowers/plans/2026-09-24-h06-manager-gui.md` checkboxes.
- Update: `Vault: evidence/H06.md` with final case matrix.

- [x] **Step 1: Run final repository gates**
  - Run `cargo test --all-targets`.
  - Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `git diff --check`.

- [x] **Step 2: Review the diff**
  - Confirm no model, secret, runtime state, generated bundle, or unrelated formatting changes are staged.
  - Confirm menu event IDs are unique and no second process is spawned.

- [x] **Step 3: Commit one reviewable H06 GUI purpose**
  - Read `CONTRIBUTING.md` before Git operation.
  - Commit implementation and tests together with an imperative subject.
  - Do not push or change H06 task state until the GUI evidence is reviewed.

## Execution Notes

- The runtime executor boundary is an explicit gate. A GUI submission that only creates a `JobJournal` entry is not a completed H06 operation.
- Existing models are never deleted or re-downloaded for this task.
- If the app build or AppKit smoke fails, record the exact failure and keep the working runtime untouched.

## Final execution note (2026-09-24)

- Tasks 1–6 completed on `feature/v040-integration` through commits `2206840`..`7b2e901`.
- `cargo test --all-targets` passed with the pre-existing `w09_all_fixtures_accepted_by_real_codex_parser` test skipped; the unskipped test requires `/Users/o/LLM/codex-v040-tmp/codex-rs/target/debug/codex-replay`, which is not present in this environment.
- Monitor suite, format, clippy, and diff check passed. GUI smoke confirmed one Manager window and close/reopen reuse. H06 remains PENDING until runtime manager jobs show terminal executor transitions.
