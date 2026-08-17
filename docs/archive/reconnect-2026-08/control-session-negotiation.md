# Control session negotiation (P0-B)

本設計は `docs/archive/reconnect-2026-08/reconnect-improvement-implementation-plan.md` の Phase G (P0-B control
session generation) の正本である。対象は G-01〜G-05 で、R0-06 で観測された「方向依存の
control generation mismatch」を解消する。詳細な観測は計画書 R0-06 Evidence を参照。

## 1. 現行の不整合

現行の pair は coordinator が自分の control generation で `Pair` を送るだけで、peer の
generation を事前に考慮しない。worker 側の `ControlProcessor::handle_validated` は
`Pair` の generation が自身より大きい場合のみ advance するため:

- coordinator 高世代: worker が advance して pair が収束する (既に方向依存を固定済み)。
- worker 高世代: coordinator の低い `Pair` を worker が `GenerationMismatch` (409) で
  拒否し、periodic reconcile を何回進めても収束しない (R0-06 RED)。

`ControlProcessor` の advance は既に存在するが、coordinator が相手の generation を
offer に反映しないため、片側再起動で generation が進んだ場合に方向依存の不整合が残る。

## 2. 基本原則

1. **coordinator を唯一の session authority とする**。session generation の決定権は
   coordinator が持ち、worker は offer を確認 (confirm) するだけである。worker が
   authority を持たない。
2. **`Pair` command を offer/confirm の両方向で使い回す**。既存 wire format を維持し、
   `ControlCommand::Pair` を coordinator からの **offer**、worker からの **confirm**
   として解釈する。新規 wire 型は追加しない。
3. **candidate generation は双方の既知値より古くない checked 値**とする。overflow は
   checked arithmetic で検出し、`u64::MAX` に達したら明示的な `GenerationExhausted`
   へ遷移する (停止条件: generation reset は行わない)。
4. **session commit は原子的**とする。local generation、peer lease、control phase、
   processed request map を一つの操作で更新し、部分更新の window を作らない。

## 3. offer/confirm の message と phase

`ControlCommand::Pair` は descriptor を持つ。sender role で意味が変わる。

| Sender | 意味 | 許可 role | 要求 phase |
|---|---|---|---|
| coordinator | **offer**: candidate generation を提示し、session を提案する | worker へ | Unpaired/Paired |
| worker | **confirm**: offer の generation を受理し、descriptor を返す | coordinator へ | Unpaired/Paired |

- offer/confirm はどちらも `ControlMessage.generation` に **candidate generation** を載せる。
- offer 受理後の worker は、confirm で同じ candidate を返す。coordinator は confirm を
  受けて session を commit する。
- idempotency: 同一 `(generation, request_id)` は既存 `processed` map により同じ response
  へ収束する (`ControlResponseStatus::Duplicate`)。offer/confirm の retry は同一 session
  へ冪等に収束する。
- 確定前 session の non-Pair command/ack と、確定済み session より古い command/ack は
  既存の generation 照合 (`GenerationMismatch`) と phase 照合 (`InvalidPhase`) で拒否する。

## 4. candidate generation の計算

coordinator は offer を送る前に peer の control generation を知る必要がある。peer の
generation は `/v1/node` (`ProductionControlClient::node()`) の応答
(`ControlResponse.generation`、即ち peer の `local_descriptor().generation`) で得られる。

```
candidate = max(local_control_generation, peer_control_generation)
```

- `max` は checked に計算し、`u64::MAX` を超えることはない (`max` 自体は overflow しない)。
- ただし `u64::MAX` に到達した session はそれ以上進めないため、`u64::MAX` を
  candidate として受け取った側は `GenerationExhausted` として扱い、それ以上 advance
  しない。現行実装は `u64::MAX` 到達時に `GenerationMismatch` を返し、明示的な
  exhaustion 検出は Phase B (failure tracker) で接続する。G 系では `max` と既存の
  `> local` による advance のみで方向非依存を実現する。
- candidate が local より大きければ、coordinator は offer 送信前に
  `advance_generation(candidate)` で自分の session generation を先に進める
  (offer の `generation` は `control_generation()` で candidate を返すため)。

## 5. session commit の原子境界

session は coordinator が confirm を受けて commit する。commit は次の操作を一つの
lock (coordinator の `RoleControl::Coordinator` の Mutex) 内で行う。

1. `advance_generation(candidate)` で local generation を更新。
2. `lease.establish` で peer lease (descriptor + generation + expires) を更新。
3. `phase = Paired` に更新。
4. `processed.clear()` で古い request map を破棄 (新しい generation の retry から
   独立させる)。

worker 側の confirm も `handle_validated` 内で同様に原子的に行われる
(advance → lease establish → processed insert)。どちらも部分更新の window を持たない。

## 6. crash point と再送の収束

| Crash point | 状態 | 再送時の収束規則 |
|---|---|---|
| offer 前 | 双方 Unpaired、session 変更なし | 次回 periodic pair が再度 offer を送る |
| offer 後 (worker 未受理) | coordinator が candidate を先に advance 済み | 再送 offer は同じ candidate。worker は初回受理 |
| offer 後 (worker 受理、confirm 送信前) | worker が candidate で advance 済み | worker が再起動/再送で confirm を再送。coordinator は `Duplicate`/再受理で commit |
| confirm 後 (coordinator commit 前) | 双方 candidate で advance 済み、lease 未 commit | 再送 confirm は idempotency で同じ response へ収束し、commit を完了 |
| confirm 後 (coordinator commit 後) | session 確定、lease 有効 | 以後の offer/confirm は同じ candidate の `Duplicate` か、stale generation の拒否 |

片側再起動では、persisted control session generation (G-03) を復元するため、再起動後に
低い generation へ戻らない。双方を再起動せずに片側だけで negotiation を再開できる。

## 7. 永続化 field と schema migration

`PersistentClusterState` に `control_session_generation` を追加する。これは cluster
generation (`generation`) とは別 field として保持し、混同しない。

- 追加 field は `#[serde(default)]` 付き optional 相当にし、旧 schema (field なし) の
  state file を引き続き読めるようにする (schema version は据え置き)。field が無い場合は
  起動時に cluster generation から初期化する。
- `persist_runtime_state` で snapshot と併せて保存し、起動時 (`ProductionClusterRuntime`
  の descriptor 初期化) に復元する。
- truncated/corrupt state は既存の `StateStore` の preserve-and-fail 動作で処理し、古い
  command を再受理しない。

## 8. 方向別の収束

| 方向 | offer generation | worker | coordinator | 結果 |
|---|---|---|---|---|
| 同世代 | local | advance なし、受理 | confirm 受理、commit | Paired |
| coordinator 高世代 | coordinator の値 | advance、受理 | そのまま | Paired |
| worker 高世代 | max (worker の値) | 変更なし、受理 | advance、commit | Paired |

いずれも有限回の periodic retry で Paired に収束し、方向に依存しない。

## 9. 停止条件

- generation reset、worker authority、古い non-Pair command の許可が必要になったら
  本設計を破棄し operator 承認を得る。
- `u64::MAX` 到達を自動復帰させる (generation reset) 設計は行わない。

## 10. 実装対象

| Task | 対象 |
|---|---|
| G-01 | 本設計書 (本ファイル) |
| G-02 | `src/cluster/control.rs` に candidate 計算と原子 commit を追加、table-driven unit test |
| G-03 | `src/cluster/state_store.rs` に `control_session_generation` を追加し永続化・復旧 |
| G-04 | `src/cluster/production/pairing.rs` の `pair()` を `/v1/node` 入力 → offer → confirm へ変更 |
| G-05 | `tests/reconnect_production.rs` の方向別 matrix + `coordinator_adopts_higher_worker_generation_on_pair` の ignore 解除 |
