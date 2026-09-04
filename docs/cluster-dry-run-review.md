# Siderostat Cluster Dry-Run Mode レビュー観点

## 1. 文書情報

| 項目 | 値 |
|---|---|
| 文書状態 | v0.1 Draft / レビュー観点 |
| 対象 | `feature/cluster-dry-run` の実装レビュー |
| 基準 | `docs/spec.md`（v0.3.0 Target Specification）、`CONTRIBUTING.md`（Git 運用・Review 基準） |

## 2. レビューの目的

dry-run モードが「クラスタリングだけを実行し、DS4 への起動/停止/再起動を行わない」という目的を、production のクラスタリング実装を書き換えずに実現しているかを確認する。加えて、dry-run が production 運用を誤って変更・汚染しないこと（安全性）を確認する。

## 3. レビュー観点

### 3.1 Target behavior と仕様書の整合

- dry-run が state machine、control plane、discovery、pairing、promotion、demotion、recovery の **production 実装をそのまま**使っているか（dry-run 専用の別経路を新設していないか）。
- クラスタリングの期待遷移（solo→pair→promote→distributed→demote）が production と同一の `ClusterEvent` と `ClusterState` で駆動するか。
- `--dry-run` フラグが `serve` に正しく配線され、`ServeOptions` を経由して `serve_with_options` に届くか。

### 3.2 実プロセス非操作の保証

- dry-run で次が**絶対に実行されない**ことを確認する。
  - `cleanup_startup_processes`（startup cleanup）
  - `reconcile_restart`（restart reconcile）
  - `ManagedChild::spawn` / SIGTERM / SIGKILL
  - `load_persisted_state` / `persist_runtime_state`（実 `StateStore` への読み書き）
- dry-run の standalone / distributed supervisor が、実 child を持たず `child_identity` が `None` を返すこと。
- dry-run で role 変更監視（`spawn_role_change_monitor`）が起動されず、実プロセス全体の再起動を伴わないこと。
- 誤って実プロセスを触る経路（例: 通常の `new()` を誤用）が dry-run に混入しないこと。

### 3.3 HELLO と route 観測のシミュレート整合

- `build_hello_frame` が `parse_hello_frame` と rendezvous 検証（`validate_worker_hello`）を通るか。
- dry-run worker の HELLO が、協調者の `WorkerHelloExpectation`（layer、context、model_name、has_output）と整合するか。
- 接続 source が `worker_address` に bind され、rendezvous の `WrongSource` 検証を満たすか。
- dry-run coordinator の route probe（`CoordinatorControlProbe`）が、production の `promote_after_hello` 前提条件と同じ式 `phase == WorkerReady && peer_present(now)` を返すか。demote / peer loss で route が正しく落ちるか。
- dry-run がネットワークモニタを起動せず、代わりに合成 `AuthenticatedPeer` 証跡（`NetworkEvidence::apply_dry_run`）で `route_scoped` を満たすか。この合成証跡が production の control plane / state machine を誤って書き換えていないか。
- dry-run の route probe が `Weak<ProductionInner>` 経由で control plane を参照し、runtime の lifetime を延長していないか。

### 3.4 Failure behavior と rollback 可能性

- dry-run の promotion 失敗（HELLO timeout、startup timeout、lease lost）が production と同じ failure action（backoff / manual / paired / solo）に従うか。
- dry-run が実プロセスを触らないため、失敗時の rollback が「dry-run を止めるだけ」で済むか（実状態を汚染しない）。
- dry-run の状態が実 `StateStore` に残らないため、後続の本番起動を誤導しないか。

### 3.5 Test evidence

- 既存テスト（`cargo test --all-targets`）が回帰しないこと。
- dry-run 固有の unit test が存在し、次を検証する。
  - `build_hello_frame` → parse / rendezvous 検証（`dry_run_hello_builds_a_frame_the_rendezvous_parser_accepts`、`built_hello_frame_round_trips_through_the_parser`）
  - dry-run standalone の start/stop/is_running / `child_identity == None`（`dry_run_standalone_toggles_running_without_a_child`）
  - dry-run worker の start/stop/is_running（`dry_run_worker_start_stop_toggles_running`）
  - dry-run coordinator の wait_ready / wait_route_loss（route probe 追従）と stop 後エラー（`dry_run_coordinator_tracks_the_route_probe`、`dry_run_coordinator_wait_ready_errors_after_stop`）
  - `new_dry_run` が合成ネットワーク証跡を注入し `route_scoped()` を満たすこと
- **2 node の dry-run 統合テスト（`tests/dry_run_cluster.rs`）で promotion/demotion が通る**（実装済み）。pair → promote → demote を実 control HTTP 経由で実行し、`running=true` / `pid=None`（実プロセス無し）を検証する。

### 3.6 Config・運用・README への影響

- `--dry-run` が開発専用の CLI フラグであり、本番設定・運用ドキュメントを誤って変更しないこと。
- README / 導入ガイドに dry-run の用途と制約（実推論を提供しない）が明記されること。
- dry-run が既定値で有効にならないこと（明示フラグでのみ有効）。

### 3.7 Secret・model・runtime artifact の非混入

- dry-run が実 model / checkpoint / secret / KV cache を読み書きしないこと。
- dry-run の HELLO に secret や prompt が含まれないこと。
- ログに秘密値・prompt・session identifier を出さないこと（spec §12）。

## 4. 完了条件

- 上記観点すべてを満たす。
- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets` が PASS。
- dry-run が実プロセスに一切触れないことをコード・テストで確認できる。


## 5. レビュー結果・修正・再レビュー記録（v0.2）

### 5.1 初回レビュー結果

- 3.1〜3.5, 3.7: 概ね OK
- 3.6: README に dry-run の用途・制約が未記載（指摘 m1）

| ID | 重大度 | 指摘 | 対応 |
|---|---|---|---|
| M1 | 中 | 2 node dry-run 統合テストが未実施（3.5「可能なら」） | 実装済み（`tests/dry_run_cluster.rs`）。pair → promote → demote を実 control HTTP で通し、実プロセス無し（`pid=None`）を検証 |
| M2 | 中 | dry-run でも `validate_dspark_binding` が実 model を読む（3.7 抵触） | `app.rs` で `if !dry_run` に変更しスキップ |
| m1 | 低 | README に用途・制約が未記載（3.6） | README に "Dry-run mode (development only)" を追記 |
| m2 | 低 | `send_dry_run_hello` の layer 42/43 がハードコード | コメントで「config の layer_start のみ厳格で 42/43 は任意のシミュレーション定数」と明記 |
| m3 | 低 | dry_run フラグの引数伝播が手渡し | 対応不要と判断（既存の decline_startup_cleanup と同様の設計） |

### 5.2 再レビュー（M1 追加実装・修正後）

- M2 / m1 / m2 の修正後に `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features`（354 passed / 0 failed）を PASS。
- **M1 統合テストが実装バグを検出**：promote 中に `peer.begin_drain()` が coordinator の control phase を `WorkerReady` → `Drained` へ進めるため、dry-run の route probe（当初 `phase == WorkerReady` のみ）が `wait_ready()` で true にならず `CompleteRouteTimeout` で失敗した。probe を `phase ∈ {WorkerReady, Drained} && peer_present(now)` に修正（§5.5 の設計更新）。
- `distributed_coordinator` を `OnceLock` 化し、dry-run の協調者 lifecycle を診断へ反映（`children.distributed_coordinator` が `running=true` / `pid=None` で観測可能）。
- `new_dry_run` に `peer_control_port` を明示引数として追加（同一ホストの 2 ノード同居テスト用。production は `config.cluster.control_port` を渡す）。
- 再レビュー結果: 3.1〜3.7 を全て満たす。完了条件（§4）を充足。
