# PeerLost recovery 設計

Status: **APPROVED-PENDING-REVIEW**

最終更新日: 2026-08-14

## 1. 目的と範囲

本書は、peer 接続断または片側再起動によって control/route lease が失効したときに、各 node
が distributed child を残さず local standalone serving へ収束する単一の recovery owner を定義
する。対象は [`reconnect-improvement-implementation-plan.md`](reconnect-improvement-implementation-plan.md)
Phase A (A-01〜A-04) の P0-A PeerLost lifecycle である。

本設計の正本は [`spec.md`](../../spec.md) の 18.4 / 18.5 節を前提とし、本書はその実装上の所有権と
順序、failure mapping を固定する。本書が spec と矛盾する場合は実装を進めず、spec を正として
本書を更新する。

## 2. 現行の不整合 (R0-05 で固定した RED)

R0-05 の RED test は次を再現する。

- 両 node が DistributedReady で distributed coordinator/worker child を稼働させた状態から、
  両 node の control HTTP server を停止して PeerLost を発火する。
- `fallback_to_solo` は state machine を SoloStandaloneReady へ進めるが、
  1. coordinator の distributed coordinator child を停止しない (orphan)。
  2. worker の distributed worker child を停止しない (orphan)。
  3. worker は standalone と distributed を同時稼働させる (coexistence)。
  4. coordinator は standalone を再起動しない。

すなわち、現行の recovery は state 遷移のみで child lifecycle と整合しておらず、
「state だけが SoloReady でも child/target/admission が不整合なら失敗させる」という
R0-05 action 4 の判定基準を満たさない。

## 3. 単一の recovery owner

control reconcile の PeerLost 検出と route-loss monitor の demotion は、同一の
`PeerLossRecovery` owner を通す。両者は別 task から発火しうるが、child の stop/start と
state 遷移を同時に行うと orphan / duplicate を生むため、直列化する。

`PeerLossRecovery` は次を保持する。

- `lock`: `tokio::sync::Mutex<()>`。recovery の stop/start/transition を直列化する。
- `completed_generation`: `AtomicU64`。直近に SoloReady まで recovery を完了した
  cluster generation を保持する。

### 3.1 取得と世代照合

`recover_from_peer_loss(owner)` の手順:

1. `lock` を取得する。
2. 現在の snapshot を読む。
3. `state == SoloStandaloneReady` なら冪等 no-op として `Ok(current)` を返す。
   これは「同じ generation の重複 recovery」が二重に child を操作しないことを保証する。
4. `current.generation <= completed_generation` なら「古い generation の recovery」として
   no-op を返す。state machine の `expected_generation` 照合と併せ、古い recovery が新しい
   state/child を操作しない。
5. そうでなければ 3.2 の順序で recovery を実行し、完了後に
   `completed_generation = ready.generation` を記録する。

state machine の `apply` は `expected_generation` で世代を照合し、古い event を
`TransitionError` で拒否する。したがって世代保護は owner の lock と state machine の二重で
担保される。

## 4. role 別の実行順序

両 role とも次の順序で実行する。順序は入れ替えない。

1. `admission.block()` と `proxy target = Unavailable(Transition)` で、今後の request を
   admission で受けない。これは `reconcile_local` と同じ前置条件である。
2. 自 node の distributed child を identity 確認付きで `stop()` する。
   - coordinator は distributed coordinator child のみ。
   - worker は distributed worker child のみ。
   - 相手 node の child は触らない (各 node が自分の child を回収する)。
3. state machine に `PeerLost` を適用して `SoloStandaloneStarting` へ進める。
4. `standalone.start(starting.generation)` を実行する。
5. state machine に `LocalStandaloneReady` を適用して `SoloStandaloneReady` へ進める。
6. `proxy target = LocalStandalone, ready=true` を publish し `admission.start_serving()` する。

worker と coordinator で child の種類と standalone の起動が異なるだけで、順序は共通である。
特に coordinator も standalone を再起動する (R0-05 action 4 の不整合 4 を解消)。

## 5. stop の identity 検証

`stop()` は `DistributedCoordinatorLifecycle::stop` / `DistributedWorkerLifecycle::stop` を
通す。production の supervisor は `ChildIdentity` (pid / executable / argv_sha256) を照合して
から signal する。identity が不明な process へは signal しない (unknown process の誤 kill を
防ぐ)。fake harness の `stop()` は running flag を落とすだけで production の identity 検証を
模擬する。

## 6. failure mapping

recovery 中の stop/start/readiness/identity 失敗は次の表で分類する。

| 失敗 | 扱い | 結果 state |
|---|---|---|
| distributed stop 成功、standalone start 成功 | 正常 | SoloStandaloneReady + Serving |
| distributed stop 失敗 (identity mismatch / kill 失敗) | retry 可能 | SoloStandaloneStarting + Unavailable のまま、次回 reconcile で再試行 |
| standalone start 失敗 | retry 可能 | SoloStandaloneStarting + Unavailable のまま、次回 reconcile で再試行 |
| state 遷移の世代不一致 (古い recovery) | no-op | 現在 state を維持 |
| standalone readiness 未達 (LocalStandaloneReady 適用失敗) | retry 可能 | SoloStandaloneStarting + Unavailable のまま、次回 reconcile で再試行 |
| 永続的に回復不能 (child を止められない、identity 不明が継続) | ManualInterventionRequired | ManualInterventionRequired |

retry 可能な failure は、state を偽の Ready に進めず `SoloStandaloneStarting + Unavailable` を
維持し、periodic reconcile が再試行する。これは「state だけ Ready の偽装」をしないという
R0-05 action 4 を満たす。`ManualInterventionRequired` は B 系 task の failure tracker と
連携し、operator reconcile で解除する (Phase B で接続)。

現行実装では、`PeerLost` 適用と `standalone.start` が失敗した場合は error を返し、
recovery owner の lock を解放して次回 reconcile に委ねる。`LocalStandaloneReady` 適用失敗も
同様に retry する。

## 7. route-loss monitor との直列化

coordinator の `promote()` が起動する route-loss 監視 task は、DS4 route が失効して grace を
超えたとき:

- peer control がまだ有効 (worker 到達可能) なら従来どおり graceful demote で
  PairedStandaloneReady へ戻す。
- peer が失効 (peer loss) なら `recover_from_peer_loss(RouteLossMonitor)` を通し、reconcile と
  同じ owner で SoloStandaloneReady へ収束する。

`recover_from_peer_loss` は owner を取得するため、control reconcile と route-loss が同時に
発火した場合は片方が完了するまで待ち、後発は「既に Solo」または「世代不一致」で no-op に
なる。これにより A-04 の同時発火 race でも child の二重操作が起きない。

## 8. 冪等性と重複

- 同一 generation の重複 recovery: 最初の完了後は `state == SoloStandaloneReady` なので no-op。
- 古い generation の recovery: `current.generation <= completed_generation` または state machine
  の世代照合で no-op。
- promotion 中の PeerLost (AwaitingWorkerHello / Promoting / DistributedStarting):
  distributed child がまだ起動していない/起動途中の場合、`stop()` は identity が無ければ
  no-op で成功する。child が起動済みなら identity 照合の上で停止する。

## 9. 対象外

- promotion failure の Backoff / ManualInterventionRequired への自動遷移は Phase B で接続する。
- network gate (N 系) は本設計の対象外。
- timeout の無制限化、assertion 緩和、stability sleep 延長は行わない。
