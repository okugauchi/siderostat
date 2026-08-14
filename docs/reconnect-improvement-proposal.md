# 再pair（reconnect）改善提案

> 対象: peer 接続断 → Solo Standalone → 再pair → Distributed 復帰の自動経路。
> 本稿は現行実装の調査結果（[`connection-state-machine.md`](connection-state-machine.md)）に基づく
> 原因候補と、優先度付きの改善案である。ユーザー報告とコード上の欠陥候補を区別し、
> 再現テストで確認できていないものは仮説として扱う。
> 最終更新日: 2026-08-14

## 1. 症状と目標

ユーザー報告:

- 一度確立した pair の peer が接続断になった後、Distributed へ自動復帰した実績がない。
- 2 台とも macOS を再起動したときは Distributed へ復帰できる。
- 片側だけの再起動では復帰できない。

本提案の完了条件は、単に control の `/v1/pair` が 200 を返すことではない。次を一連の
reconnect として検証する。

1. peer loss 後、両 node が実際に Solo Standalone を serving する。
2. stale distributed child や orphan transition が残らない。
3. 再接続後、Paired Standalone へ収束する。
4. auto promotion により新しい有効な generation で DistributedReady へ戻る。
5. state、proxy target、admission、child identity が一致する。

## 2. 現行の自動経路

### 2.1 control と pairing

```text
各 node の periodic reconcile（既定 5 秒）
  -> GET /v1/node
  -> 失敗時は PeerLease.route_scoped=false
  -> reconcile_peer()

coordinator が SoloStandaloneReady かつ auto_pair=true
  -> pair()
      -> POST /v1/pair (generation = control_generation())
      -> worker が受信して Pair を返信
      -> 双方の PeerLease を establish
      -> stability 待ち後に双方が reconcile_peer()
      -> PairedStandaloneReady
  -> auto_promote=true なら promote()
  -> DistributedReady
```

`pair()` 自体は coordinator 起点に限定される。worker の periodic task も SoloReady で
`pair()` を呼ぶが、role check で失敗する。

### 2.2 process 再起動時の generation

再起動時は永続 state の `generation` を baseline とし、Booting から SoloReady までの
2 遷移を行う。その後 `ProductionInner.descriptor` と `ControlProcessor.local` が作られる。
したがって process 再起動は control generation を 0 へ戻さず、通常は再起動前より大きい
初期値を作る。

起動後の control generation は cluster generation と独立しており、より大きい Pair を
受信した場合にだけ更新される。通常の cluster transition や Pair の成功回数ごとに
control generation が増えるわけではない。

### 2.3 DistributedReady での peer loss

periodic control reconcile が peer loss を検出すると、`ModeRuntime::fallback_to_solo()` が
直接呼ばれる。この経路は state を SoloReady へ進めるが、distributed lifecycle の cleanup を
行わない。

- coordinator / worker の distributed child を停止しない。
- coordinator standalone を起動しない。
- worker は standalone を起動するが、distributed worker child が残り得る。
- coordinator route-loss demotion task は、state が先に DistributedReady 以外へ進むと
  cleanup せず終了する。

このため、再pair handshake が成功しても、その後の serving や再promotionが正しく完了するとは
限らない。

## 3. 原因候補の評価

### 3.1 【P0候補】PeerLost recovery の lifecycle 不整合

コード上で確認済みの欠落であり、最優先で再現テストを作る。

state は SoloStandaloneReady でも、実 child は次のいずれかになり得る。

- coordinator: standalone 停止、distributed coordinator 残存。
- worker: standalone と distributed worker が併存。

この不整合は、ユーザーが観測する「pair に戻らない」に次の形で寄与し得る。

- SoloReady と表示されても local standalone service が成立していない。
- 再promotion時に以前の distributed child が no-op start で再利用される。
- child generation と新しい cluster transition generation が一致しない。
- route/admission の古い状態が次の promotion に持ち越される。

これは有力原因だが、実機症状との因果関係は child PID、profile、generation、route log を
採取して確定する。

### 3.2 【P0候補】control generation の方向依存 mismatch

低い generation の Pair が高い generation の受信側へ送られると、受信側は
`GenerationMismatch` を返す。Pair は大きい generation への追従だけを許し、下げる方向の
同期手段はない。

`control_generation()` は active な自ノード世代ではなく、既存 PeerLease に descriptor が
あれば最後に受信した peer descriptor の generation を優先する。このため coordinator が
古い peer descriptor を保持したまま worker だけが再起動すると、次の形になり得る。

| 条件 | 予想される動作 |
|---|---|
| worker だけ再起動し `worker_generation > coordinator_generation` | coordinator の低い Pair が worker で拒否され、周期試行だけでは回復不能になり得る |
| coordinator だけ再起動し `coordinator_generation > worker_generation` | worker は高い Pair を受けて generation を進められるため、この要因だけなら回復可能 |
| cable blip、process 再起動なし、双方同一 generation | generation mismatch は通常発生しない |
| 両 node 再起動 | 永続 generation と各 node の遷移回数次第。0 への reset ではない |

したがって generation mismatch は実在する設計欠陥だが、「片側再起動はすべて失敗する」ことや
「両側再起動だけが generation を reset する」ことの説明にはならない。再起動方向別テストと
409 応答の `expected` / `received` 採取が必要である。

### 3.3 【P1候補】Backoff / operator reconcile の production 未配線

`CoordinatorDistributedRuntime::reconcile_backoff()` は実装されているが、production periodic
task から呼ばれない。このため Backoff は timeout 後も自動復帰しない可能性がある。

また admin reconcile は state machine へ `OperatorReconcile` を直接適用し、failure tracker の
reset を行う `CoordinatorDistributedRuntime::operator_reconcile()` を迂回する。

promotion failure のうち tracker に接続されている種類も限定的である。再pair後に promotion が
一度失敗したケースを「reconnect失敗」として観測している可能性があるため、state と failure
status を分けて記録する。

### 3.4 【P1候補】production 境界を覆うテスト不足

既存テストには次がある。

- `phase5_security`: route detach/attach と Solo ↔ Paired の10回反復。
- `phase4_distributed`: Paired ↔ Distributed の10回反復と failure recovery。
- control unit test: stale generation の拒否。

ただし、これらは次を一つのシナリオとして連結していない。

- production control HTTP と Pair 返信 side effect。
- 永続 generation を使った片側 process 再生成。
- DistributedReady 中の peer loss。
- distributed child cleanup と standalone readiness。
- 再pair後の auto promotion。

低レベルテストは存在するが、今回の故障境界を検証する E2E がない、というのが正確な評価である。

### 3.5 【低優先度仮説】Pair 返信 timing

coordinator の最初の `pair()` 内の stability sleep が、worker からの返信 Pair 処理より先に
満了する可能性はある。しかし SoloReady で `peer_present=false` の reconcile は
`fallback_to_solo` を起こさない。また返信 Pair の受信 side effect も別途 sleep 後に
reconcile を行う。

したがって一時的に pairing が遅れる可能性はあるが、現時点で恒久的な reconnect failure の
主因とする根拠はない。計測で未収束が再現された場合にのみ改善対象とする。

## 4. 提案する改善

### P0-0: 失敗する production 相当テストと診断情報を先に作る

修正前に、少なくとも次を観測できるテストまたは実機記録を作る。

- 両 node の cluster generation / control generation。
- Pair 409 の expected / received generation。
- state、stable mode、proxy target、admission。
- standalone / distributed child の PID、profile、generation、生存状態。
- PeerLost、route-loss demotion、Pair、promotion の開始・完了順序。

テストは「最終 state だけ」でなく、不要 child が停止され、必要 child が readiness を満たしたことを
assert する。

### P0-A: peer loss recovery を role 別 lifecycle として実装する

DistributedReady または promotion 中の PeerLost を、汎用 `ModeRuntime::fallback_to_solo` だけで
処理しない。

1. future admission を block する。
2. coordinator / worker の distributed child を identity 確認付きで停止する。
3. local standalone を起動し readiness を待つ。
4. readiness 成功後にだけ SoloStandaloneReady と target=LocalStandalone を publish する。
5. 途中失敗時は ready state を偽装せず、Unavailable または ManualInterventionRequired とする。

control reconcile と route-loss demotion が同じ child を並行操作しないよう、単一の recovery owner、
または transition generation による排他を設ける。

### P0-B: Pair 用 session generation を明示的に再ネゴシエーションする

cluster generation と control session generation は用途が異なるため、単純な1系統統合や
peer loss 時の 0 reset は行わない。次の性質を持つ handshake を設計する。

- coordinator が pairing session の authority である。
- `/v1/node` 応答ですでに得られる peer generation を negotiation の入力にできる。
- 新 session generation は双方の既知値より古くならない。必要なら checked `max + 1` を使う。
- generation 確定前の Pair offer と、確定後の Pair confirm を区別する。
- session 確定時に lease、control phase、idempotency map を一貫して更新する。
- 古い non-Pair command / ack は引き続き拒否する。
- process crash 後も authority と session の意味が失われないよう、必要な値を永続化する。

最小変更として coordinator が `/v1/node` の peer generation を読んで追従する案は有力だが、
Pair 適用前に local generation だけを変更すると古い lease/phase と混在するため、session reset を
一つの原子的操作として定義する。

worker 起点 Pair を許可するだけでも高い worker generation を coordinator へ伝えられるが、
pairing authority の二重化と同時開始 race が増える。この案は第一選択にしない。

### P0-C: 再接続シナリオを自動テストする

| 開始状態 | 障害 | 必須確認 |
|---|---|---|
| PairedStandaloneReady | cable blip | 両 node Solo → 再pair → Paired |
| DistributedReady | cable blip | distributed child 停止 → 両 node local standalone → 再pair →新規 Distributed |
| Paired / Distributed | coordinator process のみ再起動 | generation 収束、orphan なし、自動復帰 |
| Paired / Distributed | worker process のみ再起動 | coordinator 低世代の場合を含め generation 収束 |
| Paired / Distributed | 両 process 再起動 | 永続 generation から収束 |
| 任意 | Pair 応答遅延・重複 | 同一 session へ冪等に収束 |

各ケースで cluster state だけでなく、child identity、generation、target、admission、lease、
control phase を両 node について検証する。

### P1: Backoff と manual reconcile を production に接続する

- periodic task から coordinator の `reconcile_backoff(now)` を呼ぶ。
- admin reconcile は coordinator runtime の `operator_reconcile()` を経由し、tracker を reset する。
- `failure_action()` が `PromotionBackoff` とする Hello timeout、unknown DS4 schema、
  coordinator startup timeout を tracker へ一貫して接続する。unknown schema を
  Paired 維持とする `docs/spec.md` 31節との方針差も同時に解消する。
- Backoff 中に peer loss が起きた場合の優先順位を定義する。

### P2: route / discovery の実測を pairing gate へ接続する

production handler が `route_scoped=true` を固定で渡す状態を解消し、network snapshot または
検証済み discovery candidate の世代と紐付ける。Bonjour の存在だけを trust せず、固定 source、
HMAC、bridge0 scoped route、lease、stability の全条件を満たした場合だけ peer present とする。

### P3: Pair timing の明示的完了通知

P0〜P2 後も timing 起因の遅延が再現する場合、sleep ベースではなく lease establishment の
watch/notify または Pair confirm 完了を await する。単なる sleep 延長は採用しない。

## 5. 実装順序

1. P0-0 の失敗テストを作り、generation mismatch と child lifecycle を別々に再現する。
2. P0-A で peer loss 後の child/state/target 整合を直す。
3. P0-B で片側再起動後の control session negotiation を直す。
4. P0-C の全シナリオと既存の10回反復テストを通す。
5. P1 の backoff/manual 配線を直し、promotion failure 後の自動復帰を検証する。
6. P2/P3 を network correctness と安定化として実施する。
7. Thunderbolt 直結2 nodeで spec 32.5 の実 cable 着脱と片側再起動を確認する。

## 6. トレードオフ

| 案 | 利点 | コスト / リスク |
|---|---|---|
| PeerLost lifecycle の専用化 | state と実 child の不整合を解消し、再promotionの前提を回復 | cleanup失敗、route-loss demotionとの競合を設計する必要 |
| coordinator-authoritative session negotiation | 片側再起動と世代差を方向非依存で解決 | control protocol変更。lease/phase/idempotencyの原子的更新が必要 |
| `/v1/node` generation の利用 | 既存応答を使え、追加 discovery が不要 | responseを読むだけではsession確定にならない |
| worker 起点 Pair | worker高世代を伝えやすい | authority二重化、同時Pair race、role設計変更 |
| cluster/control generation 統合 | 表面的にはカウンタが減る | nodeごとの遷移回数が異なるため、独立したcontrol session用途には不適切 |
| generation reset | 実装は単純 | stale command/ackの再受理リスクがあり採用不可 |

## 7. 文書・仕様との関係

- 現行実装の詳細と既知差分は [`connection-state-machine.md`](connection-state-machine.md) を参照する。
- target behavior は `docs/spec.md` 13、16、18、31、32.5節を正とする。
- 実装時は PeerLost recovery、control session generation、Backoff wiring を別の責務として扱う。
- 実装変更後は、両文書の「現行実装」部分を新しい配線に合わせて更新する。
