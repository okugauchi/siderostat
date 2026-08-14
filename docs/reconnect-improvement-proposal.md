# 再pair（reconnect）改善提案

> 対象: peer 接続断 → Solo Standalone → 再pair → Distributed 復帰の自動経路。
> 本稿は現行実装の調査結果（[`connection-state-machine.md`](connection-state-machine.md)）に基づく
> 根本原因の仮説と、優先度付きの改善案である。実装変更の可否は別途判断する。
> 最終更新日: 2026-08-14

## 1. 症状の要約

ユーザー報告:

- 一度確立した pair のどちらかの peer が接続断になった後、**再pair に一度も成功していない**。
- **2 台とも macOS を再起動したときだけ**理想的な pair（Distributed）に復帰できる。
- 片側だけの再起動では復帰できない。

## 2. 現状の再pair 自動経路

再pair は次の「coordinator 起点」の周期的試行だけで駆動される。

```text
coordinator (SoloStandaloneReady) の reconcile タスク
  -> auto_pair が true
  -> pair()
      -> POST /v1/pair (generation = control_generation())
      -> worker が受信
      -> worker が Pair を返信
      -> coordinator の lease が再確立
      -> 両者 reconcile_peer() -> form_pair -> PairedStandaloneReady
      -> auto_promote -> promote() -> Distributed
```

この経路は、両 node の「control 世代」が一致していることを前提にしている。

## 3. 根本原因の仮説

### 3.1 【主因】control 世代が peer 断後も再同期されず、世代差で Pair が永久に拒否される

`pair()` が送る generation は `control_generation()` で決まる。

```rust
// src/cluster/production/pairing.rs (control_generation)
RoleControl::Coordinator(control) => control.lock().await
    .peer_lease()
    .descriptor()
    .map_or(self.inner.descriptor.generation, |d| d.generation),
```

つまり送信 generation は「相手から最後に受け取った descriptor の世代」または
「起動時に固定された自ノード初期値」に依存する。一方、受信側 `handle_validated` は：

```rust
// src/cluster/control.rs (ControlProcessor::handle_validated)
if matches!(message.command, ControlCommand::Pair { .. })
    && message.generation > self.local.generation {
    self.advance_generation(message.generation);   // 大きいときだけ上書き
}
if message.generation != self.local.generation {
    return Err(ControlError::GenerationMismatch { .. });  // 一致しないと拒否
}
```

- `advance_generation` は「受信 Pair が自 local generation より**大きい**ときだけ」世代を上げる。
  世代は下がる方向には動かない。
- peer 喪失時、`invalidate_route()` は `route_scoped` を false にするだけで
  descriptor / generation を**クリアしない**。control 世代は peer 断をまたいで保持される。
- したがって、両 node の control 世代が何らかの理由でずれると（片側の process 再起動、
  再pair の繰り返しで世代がラチェット的に上昇する等）、**低い世代を持つ側が送る Pair は
  高い世代の側で GenerationMismatch により恒久的に拒否される**。
- 世代を 0 相当へ戻す唯一の手段が「両 process を起動し直す（= 両機の macOS 再起動）」であり、
  ユーザー症状と符合する。

### 3.2 【副因】pairing が coordinator 起点のみで、失敗時の世代再同期手段がない

- `pair()` 冒頭の `ensure!(role == Coordinator)` により worker は Pair を自発的に送れない。
- もし coordinator 側の control 世代が worker より低い場合、coordinator は毎周期
  「拒否される Pair」を送り続け、回復経路が存在しない。
- worker 起点の再pair、あるいは世代を再同期するハンドシェイクが実装されていない。

### 3.3 【副因】「世代」が 2 系統あり、意味が曖昧

- `ClusterSnapshot.generation`（状態機械の世代）と
  `ControlProcessor.local.generation`（control メッセージの世代）は別カウンタ。
- `pair()` は control 世代を送り、`ClusterEvent.expected_generation` は状態機械世代を使う。
- この 2 系統が同期されるタイミングが不明確で、世代差が発生しやすい。

### 3.4 【副因】再pair 経路がテストで検証されていない

- 統合テスト（`tests/phase4_distributed.rs`）は「確立済み pair からの promote/demote 往復」だけを
  検証しており、**peer 喪失 → Solo → 再pair → Distributed の自動経路はカバーしていない**。
- spec 32.5 の「cable 再接続後に自動 pairing へ収束」は acceptance criteria にあるが、
  自動テストの対象外で、実機でも一度も成功していない。

### 3.5 【副因（race）】coordinator の `pair()` と非同期の worker 返信の間の競合

- coordinator の `pair()` は `sleep(required_peer_stability)` 後に `reconcile_peer()` を呼ぶが、
  coordinator 側の lease 再確立は worker の返信 Pair が**非同期に**処理されたときに起きる。
- タイミング次第で `reconcile_peer()` が peer_present=false のまま走り、即座に
  `fallback_to_solo` へ戻る churn が起きる可能性がある（収束はする想定だが不安定要因）。

## 4. 提案する改善（優先度順）

### P0: control 世代の再同期 / 世代差耐性

Peer 断や再起動をまたいで世代差が残らないようにする。選択肢：

1. **再pair 時に世代を再同期する**
   - `Pair` を受けた側が、世代差があっても「自分が持つ最新世代を返信」する。
   - あるいは `Pair` を世代非依存の「再確立」コマンドとして扱い、establish 時に
     双方の世代を一致させてから以降の control を続行する。
2. **`control_generation()` を自ノード所有の単調世代へ変更**
   - 相手 descriptor 由来でなく、自ノードの cluster 世代（または専用の control 世代）を送る。
   - peer 断時に世代を「次の再pair 用にリセット / 再同期」する明示的遷移を状態機械へ追加する。
3. **世代不一致を致命的にせず、再同期を誘導する**
   - `GenerationMismatch` を返す代わりに、返信側が自分の世代を伝え、送信側が追従する
     ハンドシェイク（例: `Pair` に対して「現在世代 + 再同期要求」を返す）。

### P1: 再pair 経路の統合テスト追加

- peer 断 → Solo Standalone → 再pair → Paired Standalone → promote → Distributed を
  自動テストで回す。
- 特に「coordinator だけ再起動」「worker だけ再起動」「両方再起動」「ケーブル blip」の
  4 パターンで世代差が残らないことを確認する。
- このテストが先に落ちることで、P0 の妥当性を検証できる。

### P2: pairing 起点の非対称性を解消（または世代再同期を coordinator 側に集約）

- 現状は coordinator 起点固定のため、coordinator 側が低世代のときに回復不能になる。
- worker 起点の再pair、または世代再同期を coordinator 側の失敗時に worker が誘導する仕組みを検討する。

### P3: race の解消

- `pair()` 内の `sleep → reconcile_peer()` を、lease 再確立の完了を待ってから実行するようにする。
- coordinator 側 lease の再確立を `pair()` 自身が確認できるようにする（返信 Pair の処理完了を await）。

### P4: 世代の意味を明確化

- control 世代と cluster 世代の関係を仕様へ明記する。
- 可能なら 1 系統に統合するか、少なくとも「どちらをいつ使うか」をコメント/仕様で固定する。

## 5. 各案のトレードオフ

| 案 | 利点 | コスト / リスク |
|---|---|---|
| P0-1 再pair 時に世代再同期 | 最小変更で症状を解消できる可能性が高い。既存 handshake を拡張 | control プロトコルの変更。冪等性（processed map）との整合を慎重に設計する必要 |
| P0-2 control 世代を自ノード単調世代へ | 世代の意味が明確になり、再発しにくい | 変更範囲が広い。peer 断時の世代リセット方針の設計が必要 |
| P0-3 世代不一致を再同期で解決 | 恒久的拒否を構造的に排除 | 世代の「真実の源」を決める必要があり設計判断が要る |
| P1 テスト追加 | 回帰防止の土台。コスト低 | テストの設計（fake peer / fake DS4）に時間がかかる |
| P3 race 解消 | 安定性向上 | 小さめの変更 |

## 6. 推奨アプローチ

1. まず **P1 の再pair 統合テスト**を追加し、現状が「coordinator 再起動 / 世代差」で
   落ちることを確認する（再現性の確保）。
2. その上で **P0-1（Pair 再同期）または P0-3（世代差の再同期誘導）** で最小変更の修正を行う。
3. 修正後、P1 の全パターンと、既存の promote/demote 10 往復テストが通ることを確認する。
4. P2/P3/P4 は安定化・明確化として後続対応に回す。

## 7. 検証計画

- ユニット: `ControlProcessor` の世代再同期ロジック（世代差・冪等性）。
- 統合: 上記 4 パターンの再pair 自動経路。
- 実機: Thunderbolt 直結 2 node で、片側再起動・ケーブル着脱を想定した再pair を確認。
  spec 32.5 の「2 回連続の実 cable 着脱」と「10 回 promotion/demotion」も合わせて回す。

## 8. 現状の文書・コードとの関係

- 本提案は `docs/connection-state-machine.md`（現状把握）と対をなす。
- 実装を変更する場合は `docs/spec.md` の該当節（13, 18, 32.5）と整合を取り、
  CONTRIBUTING.md の Git 運用方針（branch、review、release gate）に従う。
