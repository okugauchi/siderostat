# Planned Restart Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 分散 `DistributedReady` 中の graceful restart を計画再起動として相手ノードと協調し、PeerLost 復旧と自動再収束の競合を解消する。

**Architecture:** `ProductionClusterRuntime` に planned-restart gate を追加する。coordinator は新しい authenticated control command `prepare-restart` で worker の gate を先に立て、通常の recovery demotion とは分離した planned-restart lifecycle で coordinator child → worker child の順に停止する。worker は lease が失効しても gate 中は reconcile/recovery を行わず、再起動後の新しい `Pair` で gate を解除する。準備後の失敗には `cancel-restart` を使い、coordinator child の停止失敗時は `Demoting` へ遷移させず再試行可能な状態へ戻す。

**Tech Stack:** Rust, Tokio, Axum, authenticated control plane, existing cluster state machine and lifecycle supervisors.

**Spec:** `docs/superpowers/specs/2026-08-25-planned-restart-protocol.md`

## Global Constraints

- 既存の PeerLost recovery と突然終了時の動作を変更しない。
- control command は deployment、generation、HMAC、idempotency の既存検証を通す。
- child identity mismatch、drain timeout、peer control failure では成功扱いにしない。
- 既存の未コミット変更を上書きしない。
- docs は秘密情報・実機固有の識別情報を記録しない。

---

### Task 1: control protocol test-first changes

**Files:**

- Modify: `src/cluster/control.rs`
- Modify: `src/cluster/coordinator/control.rs`
- Modify: `src/cluster/worker.rs`
- Modify: relevant control-plane tests

- [x] `PrepareRestart` と `CancelRestart` の endpoint、serde、deployment matching、idempotency を検証するテストを追加する。
- [x] worker control が paired/distributed phase で prepare/cancel を受理し、unpaired では拒否するテストを追加する。
- [x] coordinator の restart message 生成が許可された phase だけで成功するテストを追加する。
- [x] 先にテストを実行し、未実装のため失敗することを確認する。

### Task 2: planned-restart gate and reconcile suppression

**Files:**

- Modify: `src/cluster/production.rs`
- Modify: `src/cluster/production/reconcile.rs`
- Modify: `src/cluster/production/recovery.rs`
- Modify: `src/cluster/production/pairing.rs`
- Modify: `src/cluster/production/effects.rs`

- [x] runtime に atomic planned-restart gate と begin/cancel/clear 操作を追加する。
- [x] reconcile、PeerLost recovery、route-loss monitor、auto-pair/auto-promote が gate 中に開始しないようにする。
- [x] worker で prepare/cancel command を lifecycle effect として適用する。
- [x] worker が新しい Pair を受信したときだけ gate を解除する。
- [x] gate 中の PeerLost が no-op になること、通常時の PeerLost recovery が維持されることをテストする。

### Task 3: coordinator graceful restart integration

**Files:**

- Modify: `src/cluster/production.rs`
- Modify: `src/app.rs`
- Modify: `src/app.rs` tests

- [x] coordinator が local gate → peer prepare ACK → admission drain → planned-restart child stop lifecycle の順に実行する API を追加する。
- [x] planned restart の coordinator child 停止失敗時に `Demoting` へ遷移させず、local target/admission を復元する回帰 test を追加する。
- [x] peer prepare 後の失敗時に local gate と peer gate を解除する best-effort cancel を追加する。
- [x] standalone restart は従来の lifecycle を維持する。
- [x] graceful restart の HTTP outcome に peer preparation failure を追加し、成功時だけ server restart signal を送る。
- [x] duplicate request と drain timeout で gate が残留しない回帰テストを追加する。

### Task 4: verification and evidence documentation

**Files:**

- Modify: `docs/compatibility/v0.3.0-notification-dedup.md`
- Modify: `docs/implementation-plan-v0.3.0.md`

- [x] `cargo fmt --all -- --check`、targeted tests、`cargo test --all-targets`、clippy、`git diff --check` を実行する。
- [x] planned restart の expected transition と unexpected peer-loss の対照を文書化する。
- [x] 実機 artifact の再ビルド・インストールは、コード検証完了後に別の検証手順として記録する。
- [x] 実機未検証の内容を受入 evidence と混同しない。
