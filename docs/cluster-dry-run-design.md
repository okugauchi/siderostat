# Siderostat Cluster Dry-Run Mode 設計

## 1. 文書情報

| 項目 | 値 |
|---|---|
| 文書状態 | v0.1 Draft / 設計提案 |
| 対象 | Siderostat v0.3.1 以降 |
| 目的 | ds4-server への起動/停止/再起動操作を行わずに、Siderostat ノード間のクラスタリングだけを実行する dry-run モードの設計 |

## 2. 背景と目的

Siderostat は mode-aware reverse proxy であり、DS4 child process と 2 node cluster の lifecycle を管理する。クラスタリング（discovery、control plane、state machine、pairing、promotion、demotion、recovery）は、実際の DS4 推論プロセス制御（spawn / SIGTERM / SIGKILL / readiness 確認）と独立に設計されている。

今後の機能開発では、ds4-server 自身に Siderostat の開発を担わせる（self-hosting / self-development）可能性を検討する。その際、Siderostat のクラスタリングロジックを検証・開発するために、重い実 DS4 推論プロセスを起動・停止・再起動することなく、ノード間クラスタリングだけを安全に実行できるモードが有用である。

本設計の目的は、この dry-run モードを、既存の production クラスタリング経路をそのまま使い、DS4 プロセス制御だけをシミュレートする形で追加することである。

## 3. 設計原則

1. **クラスタリング本体は production と同一のコードを実行する。** dry-run は state machine、control plane、discovery、pairing、promotion、demotion、recovery の実装を一切書き換えない。
2. **DS4 プロセス制御だけを差し替える。** 既存の trait 抽象（`LocalStandaloneLifecycle`、`DistributedWorkerLifecycle`、`DistributedCoordinatorLifecycle`）を利用し、dry-run 用のシミュレート lifecycle を注入する。
3. **実プロセスに一切触れない。** startup cleanup、restart reconcile、DS4 child の spawn / stop / kill、ログ監視、状態永続化（実 state への書き込み）を dry-run では行わない。
4. **HELLO と route 観測はシミュレートする。** promotion は実 DS4 wire HELLO に依存するため、worker 側 dry-run lifecycle が協調者の rendezvous listener へ模擬 HELLO を送り、協調者側 dry-run lifecycle は control plane の phase / lease を route の代わりに観測する。
5. **fail-closed を維持する。** dry-run で未知状態・不一致が起きた場合、production と同じ failure action に従い、実プロセスへの影響を伴わない範囲で安全に収束する。

## 4. クラスタリングの対象範囲

dry-run が実行する production 経路は次のとおり。DS4 プロセス制御以外は production 実装をそのまま使う。

| 経路 | 実装 | dry-run での扱い |
|---|---|---|
| bridge0 / Bonjour 観測（network evidence, `route_scoped`） | `network_events.rs` / `network_evidence.rs` / `bonjour.rs` | モニタは起動しない。dry-run では合成の `AuthenticatedPeer` スナップショットを注入して `route_scoped` を満たす（下記 §5.5b 参照） |
| control plane（HMAC 認証、lease、Pair/PrepareWorker/Drained/WorkerEvent/Demote 等） | `control.rs` / `coordinator/control.rs` / `worker.rs` | 実装をそのまま実行 |
| cluster state machine（solo→pair→promote→distributed→demote、backoff、manual） | `state.rs` / `runtime.rs` | 実装をそのまま実行 |
| pairing（offer/confirm、generation 交渉、lease） | `production/pairing.rs` | 実装をそのまま実行 |
| promotion（rendezvous HELLO、worker prepare、complete route） | `production/pairing.rs` / `coordinator.rs` / `worker.rs` | HELLO 送信と route 観測のみシミュレート |
| demotion / recovery（peer loss、deployment mismatch） | `production/recovery.rs` / `coordinator.rs` | 実装をそのまま実行（プロセス停止はシミュレート） |
| 起動時 startup cleanup / restart reconcile | `startup_cleanup.rs` / `restart.rs` | **実行しない**（実プロセス停止を回避） |
| 状態永続化（`StateStore`） | `state_store.rs` / `app.rs` | **実 state へ書き込まない** |

## 5. 構成要素

### 5.1 CLI と配線

- `siderostat serve --dry-run` を追加する。`ServeOptions` に `dry_run: bool` を追加し、`serve_with_options` へ通す。
- dry-run 時は次をスキップする。
  - `cleanup_startup_processes`（実 siderostat / ds4 プロセスを触らない）
  - `reconcile_restart`（実 DS4 child を停止しない）
  - `load_persisted_state` / `persist_runtime_state`（実 state を読まない・書かない）

### 5.2 Standalone シミュレート

`StandaloneSupervisor` は app 層で具体的型として使われるため、型を変えずに dry-run フラグを追加する。

- `StandaloneSupervisorInner` に `dry_run: bool` と `running: AtomicBool` を追加。
- 専用コンストラクタ `StandaloneSupervisor::new_dry_run(...)` を追加（`dry_run: true`）。既存の `new` は `dry_run: false` のまま。
- `start_inner` / `stop_inner` / `is_running_inner` の冒頭で、dry-run なら `running` フラグを反転して早期 return（spawn / SIGTERM / readiness を伴わない）。
- `child_identity` は dry-run では `None`（実 child は存在しない）。

### 5.3 Distributed worker シミュレート

`DistributedWorkerLifecycle` の dry-run 実装を新規モジュール `src/cluster/dry_run.rs` に追加する。

- `DryRunWorkerLifecycle` は `running: AtomicBool` を持つ。
- `start(generation)` は `running = true` にし、その後 async task で協調者の rendezvous listener へ接続し、模擬 HELLO frame を送信する。
- `stop` は `running = false` にする。
- `is_running` は `running` を返す。
- `child_identity` は `None`。

### 5.4 Distributed coordinator シミュレート

`DistributedCoordinatorLifecycle` の dry-run 実装を同じく `dry_run.rs` に追加する。

- `DryRunCoordinatorLifecycle` は `running: AtomicBool` と `route_probe: Arc<dyn DryRunRouteProbe>` を持つ。
- `start` は `running = true`。
- `wait_ready` は route probe が `true`（worker が Ready かつ peer が存在）になるまで待つ。`running` が `false` になったら early-exit エラー。
- `wait_route_loss` は route probe が `false`（worker が Ready でなくなる、または peer 喪失）になるまで待つ。
- `stop` は `running = false`。
- `is_running` は `running`。
- `child_identity` は `None`。

### 5.5 Route probe（協調者の control plane を観測）

production の協調者 supervisor は DS4 log から route を観測するが、dry-run には log が無い。代わりに control plane の状態を route の代理として観測する。

- `DryRunRouteProbe` trait（`fn probe(&self) -> BoxFuture<'static, bool>`）を定義。
- 実装 `CoordinatorControlProbe` は `Weak<ProductionInner>` を保持し、`RoleControl::Coordinator` の route 完了条件 `phase ∈ {WorkerReady, Drained} && peer_present(now)` を返す。
  - これは production の `CoordinatorDistributedRuntime::promote_after_hello` が使う前提条件（`phase == WorkerReady && peer_present(now)`）を踏襲する。ただし `promote_inner` は `coordinator.wait_ready()` をポーリングする**前**に `peer.begin_drain()` を実行するため、その時点の control phase は `WorkerReady` から `Drained` へ進んでいる。2 node 統合テスト（§9）でこの不一致を検出し、`Drained` も route 完了として受理するようにした。demote / peer loss では phase が `Paired` へ戻るため `wait_route_loss` が正しく発火する。
- `ProductionClusterRuntime::finish_coordinator` が dry-run 時のみ、この probe を持つ `DryRunCoordinatorLifecycle` を構築し、`CoordinatorDistributedRuntime` へ渡す。
- `CoordinatorControlProbe` は `ProductionInner` の private フィールド（`control`）に触れるため `production.rs` 内に定義する（`dry_run.rs` には置かない）。dry-run の協調者 lifecycle（`coordinator_child`）は `new_inner` では `None` のままにし、`finish_coordinator` が `Weak<ProductionInner>` 経由で probe を構築してから `CoordinatorDistributedRuntime` を組み立てる。

### 5.5b 合成ネットワーク証跡（route_scoped）

production の control plane は `route_scoped` を `NetworkEvidence` から導出し、fail-closed で `false` の間は `establish`/`renew` を `RouteNotScoped` で拒否する。dry-run はネットワークモニタを起動しないため、このままだと pairing 自体が成立しない。

- `NetworkEvidence::apply_dry_run(role, local_address, expected_peer_address)` を追加。合成の `AuthenticatedPeer` スナップショット（`epoch = u64::MAX`）を注入し、`route_scoped()` と `peer_present()` を `true` にする。
- `ProductionClusterRuntime::new_dry_run` が構築後にこれを呼ぶ（role に応じて local/peer を入れ替え）。
- これにより、実インターフェースを観測せずに production の control plane / state machine をそのまま通す。

### 5.6 HELLO 模擬送信

`ds4_hello.rs` に HELLO frame の builder（`build_hello_frame`）を追加する（現状 parser のみ）。

- dry-run worker は config から次の HELLO を構築する。
  - `layer_start` = `distributed.worker_layers` の開始
  - `layer_end` / `layer_count` = `layer_end + 1 == layer_count` を満たす値
  - `has_output` = true
  - `context_size` = `distributed.context_size`
  - `model_name` = `manifest.model_family`
  - `listen_port` = `ds4_distributed_port`
- 接続は `TcpSocket` で source を `worker_address` に bind し、`coordinator_address:ds4_distributed_port` へ接続する（rendezvous listener の source 検証に整合）。

### 5.7 ProductionClusterRuntime の dry-run 構築

- `ProductionClusterRuntime::new_dry_run` を追加。`new` と同型の引数に加えて `peer_control_port` を明示的に受け、worker には `DryRunWorkerLifecycle`、協調者には `None`（`finish_coordinator` で構築）を注入して `new_inner` を呼ぶ。
  - `new` / `new_dry_run` は production 構成では peer の制御ポートが `config.cluster.control_port` と同値（両ノードが同じポートで listen）という前提で `peer_control_port` を config から導出していた。2 node を同一ホストに同居させる統合テストではノード毎に異なる制御ポートを使う必要があるため、`peer_control_port` を明示引数に分離した（`new_with_lifecycles` と同じ設計）。production 呼び出しは `config.cluster.control_port` をそのまま渡す。
- dry-run の協調者 lifecycle は `finish_coordinator` 内で構築される（route probe が `Weak<ProductionInner>` を要する）ため、`distributed_coordinator` フィールドは `Option` ではなく `OnceLock` にし、`finish_coordinator` が構築後に格納して診断（`children.distributed_coordinator`）へ反映できるようにする。
- `ProductionInner` に `dry_run: bool` を追加。
- `new_inner` に `dry_run: bool` を追加し、`finish_coordinator` が dry-run 時は route probe 付き `DryRunCoordinatorLifecycle` を構築する。
- ネットワーク証跡モニタ（`start_network_evidence_monitor`）は dry-run では起動しない。代わりに §5.5b の合成証跡を注入する。

## 6. 状態遷移の期待動作

dry-run でも production と同じ state machine が駆動する。

```text
Booting
  -> SoloStandaloneStarting   (BeginSoloStandalone)
  -> SoloStandaloneReady      (LocalStandaloneReady; dry-run standalone が即 ready)
peer 存在
  -> Pairing / PairedStandaloneReady (control plane Pair)
promote
  -> AwaitingWorkerHello (BeginPromotion)
  -> Promoting           (WorkerHelloAccepted; dry-run HELLO 受信)
  -> DistributedStarting (DistributedChildStarted; dry-run coordinator 起動)
  -> DistributedReady    (DistributedRouteReady; route probe true)
demote / peer loss
  -> Demoting / PairedStandaloneReady / SoloStandaloneStarting
```

## 7. 安全性

- dry-run は実プロセスを spawn / stop / kill しない。startup cleanup と restart reconcile をスキップする。
- dry-run は実 `StateStore` へ書き込まない。永続化をスキップする。
- dry-run の HELLO は模擬 frame であり、実 worker DS4 の HELLO と混同しない（source bind を worker アドレスに限定）。
- dry-run の failure は production と同じ failure action に従う。実プロセスへの影響が無いため、manual intervention はログと状態で報告される。

## 8. 実装範囲

| ファイル | 変更 |
|---|---|
| `src/cli.rs` | `serve --dry-run` 追加 |
| `src/app.rs` | `ServeOptions.dry_run`、dry-run 時のスキップ分岐、dry-run supervisor 構築 |
| `src/cluster/process/standalone.rs` | `StandaloneSupervisor` に dry-run フラグ + `new_dry_run` |
| `src/cluster/dry_run.rs` | 新規：`DryRunWorkerLifecycle`、`DryRunCoordinatorLifecycle`、`DryRunRouteProbe` |
| `src/cluster/ds4_hello.rs` | `build_hello_frame` 追加 |
| `src/cluster/network_evidence.rs` | `apply_dry_run`（合成 `AuthenticatedPeer` 証跡）追加 |
| `src/cluster/production.rs` | `new_dry_run`、`ProductionInner.dry_run`、`finish_coordinator` 分岐、`CoordinatorControlProbe` |

## 9. 検証

- `cargo test --all-targets`（既存テスト回帰）
- dry-run 用の unit test：
  - `build_hello_frame` が parse 可能で rendezvous 検証を通る
  - `DryRunStandaloneLifecycle` の start/stop/is_running
  - `DryRunCoordinatorLifecycle` の wait_ready / wait_route_loss が route probe に従う
  - `new_dry_run` が production と同一の state machine / control plane を構築する
- **2 node dry-run 統合テスト（`tests/dry_run_cluster.rs`、test-support）**：
  - 同一ホスト上に coordinator / worker の 2 ノードを `new_dry_run` で構築し、実 control HTTP 経由で pair → promote → demote を通す。worker は模擬 HELLO を協調者の rendezvous listener へ送り、協調者は control plane から route を導出する。
  - `distributed_coordinator` / `distributed_worker` が `running=true` かつ `pid=None`（実プロセス無し）、standalone も `pid=None` であることを検証する。
  - このテストは promote 中の phase `Drained` 不一致（§5.5）を検出した実装バグを発見する価値があった。
