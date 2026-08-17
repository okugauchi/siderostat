# siderostat リファクタリング提案

- 文書状態: 実装完了
- 作成日: 2026-08-11
- 完了日: 2026-08-12
- 対象baseline: `main` (`fdb3dbc` / develop fast-forward後)
- 前提: 本提案は挙動を変えないリファクタリングのみを対象とする
- 実装: `develop` 上で3.1〜3.6を各1commitで実施(3.3はproduction.rs / coordinator.rs / process.rs の3commitに分割)。Required CI(`cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets`)と `tests/phase1`〜`phase5` 統合テストをすべて通過。

## 1. 目的

現状のコードベースを調査し、保守性・型安全性・拡張時の漏れ防止の観点から、優先度の高いリファクタリング候補を整理する。本提案は仕様書(`docs/spec.md`)で定義するtarget behaviorを変更しない。

## 2. コードベース概要

- 言語/edition: Rust stable / edition 2024
- 全体規模: `src/` 約17,700行、うち `src/cluster/` が約12,000行(約7割)
- 構成: mode-aware reverse proxy / 2 node cluster supervisor

主要ファイル規模:

| ファイル | 行数 | 備考 |
|---|---|---|
| `src/cluster/coordinator.rs` | 1616 | CoordinatorDistributedRuntime + CoordinatorControl |
| `src/config.rs` | 1370 | 設定型 + 大量の検証関数 |
| `src/proxy.rs` | 1368 | プロキシ本体 + peerセキュリティ |
| `src/cluster/production.rs` | 1326 | ProductionClusterRuntime impl約800行 |
| `src/cluster/process.rs` | 1235 | 3つのsupervisor + ProcessController |
| `src/app.rs` | 1207 | serve関数約300行 + ルーター |

## 3. リファクタリング候補

### 3.1 名前変換関数の重複(優先度: 高・低リスク)

enum→文字列のマッピングが複数ファイルに重複している。

| 変換関数 | 重複箇所 |
|---|---|
| `cluster_state_name` | `app.rs` L633 と `metrics.rs` L414 |
| `stable_mode_name` / `cluster_mode_name` | `app.rs` L617 と `metrics.rs` L432 |
| `model_variant_name` | `app.rs` L955 と `metrics.rs` L441 |
| `residency_name` | `app.rs` L963 と `metrics.rs` L449 |
| `role_name` | `app.rs` L625 |

**提案**

- `src/target.rs` の各enum(`StableMode` / `ClusterState` / `LocalRole` / `ModelVariant` / `Residency`)に `name()` / `as_str()` メソッドまたは `Display` implを追加して一元化する。
- `app.rs` / `metrics.rs` の同名関数を削除し、enumのメソッドを呼ぶ形に置き換える。

**期待効果**: 重複除去。状態variant追加時の更新漏れを防止。

### 3.2 状態遷移名の型安全性(優先度: 高・低リスク)

- `metrics.rs` L402 `transition_name` は `"solo-standalone-ready"` 等のハードコード文字列を引数に取り、`ClusterState` を文字列化してからマッチする。
- `state.rs` L339 `cluster_event_name` は `ClusterEventKind` + `ClusterState` のenumベースで、遷移名の正本が2系統に分かれている。

**提案**

- 遷移名を `ClusterEventKind` + `ClusterState` のenumベースへ統一する。
- `metrics.rs` の `transition_name` は `state.rs` の `cluster_event_name` を利用する形へ整理する(必要に応じて `pub(crate)` 化)。

**期待効果**: 文字列ベースのマッチを排除し、状態追加時の更新漏れを防止。

### 3.3 巨大モジュールの分割(優先度: 中)

- `src/cluster/production.rs`: `ProductionClusterRuntime` の `impl` が約800行(L200〜1009)。ペアリング/プロモーション系、リコンサイル系、コントロール効果適用系、ワーカー管理系など、メソッド群ごとにサブモジュール化を検討。
- `src/cluster/coordinator.rs`(1616行): `CoordinatorDistributedRuntime` と `CoordinatorControl` は独立した関心であり、分割候補。
- `src/cluster/process.rs`(1235行): `ProcessController` / 3つのsupervisor / `ManagedChild` をモジュール単位で分離。

**提案**: 関心の分離に基づき、1ファイルの責務を1つに絞る分割。挙動を変えない純粋な移動に留め、`cargo clippy --all-targets --all-features -- -D warnings` を通す。

### 3.4 3つのsupervisorの構造的重複(優先度: 中)

`StandaloneSupervisor` / `DistributedWorkerSupervisor` / `DistributedCoordinatorSupervisor` が、ほぼ同じメソッドセット(`new` / `start` / `start_inner` / `stop` / `stop_inner` / `is_running` / `is_running_inner` / `child_identity`)を持ち、`start_inner` は共通の「child spawn → log forward → slot管理」パターンを持つ。

**提案**

- 共通のchild管理ロジック(`ManagedChild` + `SupervisedChild` のslot管理、log forward起動)をtraitまたはヘルパーへ抽出する。
- ただし3者は起動条件・待機ロジック(dspark activation等)が異なるため、**共通化可能な部分のみ**抽出し、過度な抽象化は避ける。

### 3.5 `app.rs` の `serve` 関数の複雑化(優先度: 中)

`serve`(L228〜530、約300行)に、設定読込・manifest/fingerprint・状態復元・複数タスク起動(transition monitor / local monitor / reconcile / control / peer / public server / admin server)が集中している。

**提案**: 「起動前セットアップ」「モニタタスク群」「サーバ起動群」に分割し、各タスクを名前付き関数へ抽出する。

### 3.6 `config.rs` の `validate_durations` の冗長化(優先度: 低)

`validate_durations`(L402〜494)が21個のdurationを手動のタプルリストで非ゼロ検証している。パス名とフィールド参照が冗長で、duration追加時の漏れリスクがある。

**提案**: durationフィールドをまとめた参照用の小構造体を作り反復検証するか、検証用マクロで簡略化する。

## 4. 実施にあたっての指針

- 各候補は**挙動を変えない**リファクタリングとし、`docs/spec.md` のtarget behaviorを変更しない。
- 回帰検証はRequired CI(`cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets`)と、`tests/phase1`〜`phase5` の統合テストで確認する。
- Git運用は `CONTRIBUTING.md` に従い、`main` から `develop` を派生させて作業する。
- 1 commitを1つのreview可能な目的に限定し、実装と対応するtestを同じcommitへ含める。

## 5. 推奨着手順

優先度の高い順に、独立して着手可能な単位で進めることを推奨する。

1. 3.1 名前変換関数の一元化
2. 3.2 状態遷移名の型安全化
3. 3.3 巨大モジュールの分割(production.rs → coordinator.rs → process.rs の順)
4. 3.4 supervisor共通化(共通部分のみ)
5. 3.5 serve関数の分割
6. 3.6 validate_durationsの整理

各候補は独立しているため、単独でcommit・reviewできる。

## 6. 補足

- テスト基盤は充実している(`tests/phase1`〜`phase5`、`tests/support`、`src/bin/fake-ds4.rs`)。
- CI(`.github/workflows/ci.yml`)は `main` / `rewrite/mode-aware` / `develop` でRequired CIを実行する設定。
- 本提案の実施可否・優先順位は、reviewを経て決定する。
