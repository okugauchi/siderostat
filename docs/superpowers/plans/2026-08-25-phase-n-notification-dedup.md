# Phase N Notification Deduplication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** coordinator-only restart の長い stop cycle でも、recovery epoch 内の通知重複を抑制し、recovery lifecycle を通知失敗から分離する。

**Architecture:** `src/notify.rs` に通知 event と epoch context を受け取る純粋な semantic reducer を置く。通知 service は reducer の送信 decision だけを既存 notifier へ渡し、抑制数は bounded-label metrics、epoch の generation／recovery ID は structured log へ記録する。`AppState` は同じ notification service に H phase の recovery lifecycle を通知するが、送信失敗を cluster lifecycle へ返さない。

**Tech Stack:** Rust, Tokio watch channel, tracing, Prometheus text-format metrics, existing `DesktopNotifier` abstraction.

**Spec:** `docs/implementation-plan-v0.3.0.md` Phase N N-01/N-02、および `docs/research/coordinator-restart-notification-repetition-2026-08-19.md`。

## Global Constraints

- `DesktopNotificationService` は cluster state、admission、child lifecycle を変更しない。
- SoloStandaloneReady と PairedStandaloneReady は同一 epoch 内で各1回だけ送信する。
- DistributedReady、恒久 Solo、Backoff、ManualInterventionRequired、DeploymentMismatch、StandaloneRestart は抑制しない。
- worker が local standalone を準備できていない一時 PairedStandaloneReady は「ノード検出」の確定通知にしない。
- recovery ID、node 名、通知本文を metric label に含めない。
- sender failure、GUI session 不在、watch channel close は lifecycle の成功／失敗判定を変更しない。

---

### Task 1: Pure notification epoch reducer

**Files:**
- Modify: `src/notify.rs`
- Test: `src/notify.rs` unit tests

**Interfaces:**
- Consumes: cluster snapshot transition, local role/readiness, optional recovery epoch context.
- Produces: deterministic emit/suppress decision and suppression metadata for the notification service.

- [x] Write table tests for 180-second-equivalent Solo/Paired repetition, worker temporary Pairing, cable-detach Solo, DistributedReady rollover, and unsuppressed critical states.
- [x] Run the notify unit test target and verify the new cases fail for the missing reducer behavior.
- [x] Implement the minimal reducer and transition mapping without changing cluster state machine behavior.
- [x] Run the targeted tests and verify all reducer cases pass.
- [x] Refactor only after the targeted tests are green; retain the pure decision boundary.

### Task 2: Service and observability integration

**Files:**
- Modify: `src/notify.rs`
- Modify: `src/app.rs`
- Modify: `src/metrics.rs`
- Test: related unit and async integration tests

**Interfaces:**
- Consumes: Task 1 reducer decisions and H recovery started/completed/failed lifecycle events.
- Produces: notification delivery, bounded suppression counters, and redaction-safe epoch logs.

- [x] Add failing tests for bounded suppression metrics, sender failure, GUI session absence, watch channel close, and recovery epoch sharing.
- [x] Run those tests and verify they fail for the missing service integration.
- [x] Connect the shared service to recovery start/completion/failure and record bounded metrics plus structured epoch context.
- [x] Keep notifier and watch-channel errors asynchronous and non-fatal to lifecycle tasks.
- [x] Run targeted tests, then the project Rust test/lint/format/diff gates.
- [x] Update `docs/implementation-plan-v0.3.0.md` with redacted N-01/N-02 evidence and stop before N-03 practical review.
