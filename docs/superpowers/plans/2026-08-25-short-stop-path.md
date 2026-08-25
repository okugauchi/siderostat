# Short Stop Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 分散モードの graceful restart が単体用 supervisor を迂回して長い再収束を起こす問題を解消し、停止要求から LaunchAgent による再起動までを lifecycle owner の cleanup を含む正常終了経路に統一する。

**Architecture:** `/admin/restart` は `ProductionClusterRuntime` が存在する場合にその runtime の planned-restart lifecycle を使って distributed child を停止し、standalone の場合だけ `StandaloneSupervisor` を停止する。distributed planned restart は通常の recovery demotion と分離し、coordinator child の停止成功後に peer worker child を停止する。成功後は `std::process::exit` を直接呼ばず、`run_servers` に restart signal を通知する。`run_servers` は通常の termination cleanup と同じ順序で listener、reconcile/monitor task、production child、standalone child を停止してから戻り、LaunchAgent KeepAlive に再起動を任せる。

**Tech Stack:** Rust, Tokio `Notify`, Axum, existing `ProductionClusterRuntime` / `StandaloneSupervisor` lifecycle APIs.

**Spec:** C-04 graceful restart contract と N-03 の短い停止経路の実機観測。分散 child の owner を迂回しないこと、identity mismatch 時に強制 kill しないこと、HTTP 202 応答を返した後に正常終了することを満たす。

## Global Constraints

- 既存のユーザー変更（`docs/implementation-plan-v0.3.0.md`、`docs/compatibility/v0.3.0-notification-dedup.md`）を上書きしない。
- `/cluster/restart` の役割変更適用やネットワーク監視の process restart は既存挙動を維持する。
- drain timeout と child identity mismatch は従来どおり強制 kill せず、再試行可能なエラーを返す。
- graceful restart の成功応答を返す前に process を終了させない。
- 変更後も standalone、coordinator、worker の cleanup が冪等であることを保つ。

---

### Task 1: owner selection and restart signal regression tests

**Files:**
- Modify: `src/app.rs` tests

- [x] 分散 runtime が存在する場合は distributed lifecycle を選択し、standalone の場合だけ standalone supervisor を選択する純粋な回帰テストを追加する。
- [x] graceful restart 成功時に process exit ではなく `run_servers` が受け取れる restart signal が予約される回帰テストを追加する。
- [x] テストを実行し、現行実装が失敗することを確認する。

### Task 2: lifecycle-owner-aware graceful restart

**Files:**
- Modify: `src/app.rs`

- [x] `AppState` に restart signal を追加し、production runtime の有無に応じて停止処理を切り替える。
- [x] graceful restart の成功後は遅延 signal を送信し、`std::process::exit` を使わない。
- [x] `run_servers` の signal select に restart signal を追加し、通常の listener/task/production/standalone cleanup を通す。
- [x] C-04 コメントとエラー条件を実装実態に合わせて更新する。
- [x] planned restart の child 停止失敗で `Demoting / blocked` を残さず、coordinator child が残る場合は DistributedReady と serving を復元する。

### Task 3: verification and real-node reproduction guide

**Files:**
- Modify: `docs/implementation-plan-v0.3.0.md`
- Modify: `docs/compatibility/v0.3.0-notification-dedup.md`

- [x] format、diff、unit/integration tests、clippy 相当の検証を実行する。
- [x] 実機で coordinator の短い停止経路を再現する sudo 不要の観測手順を整理する。
- [x] 実機未適用の場合は、コード検証と実機検証を混同せず evidence を分けて記録する。
