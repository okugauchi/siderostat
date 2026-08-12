# siderostat デスクトップ通知 実装提案

- 文書状態: 実装済み (2026-08-12)
- 作成日: 2026-08-12
- 対象baseline: `develop` (`10ddf68` / リファクタリング完了後)
- 前提: 本提案は挙動を変えない付加レイヤであり、`docs/spec.md` で定義するtarget behaviorを変更しない。

## 1. 目的

macOS のデスクトップ通知を用いて、起動時・standalone・distributed の状態変更をユーザーへ可視化する。
状態機械・proxy・persist には一切影響させず、通知の失敗は運用に影響しない設計とする。

## 2. 調査結果

### 2.1 仕様上の位置づけ

- `docs/spec.md` にはデスクトップ通知の規定は存在しない。spec 中の「通知」はすべてネットワーク/ピア間のinternal通知
  (SCDynamicStore、IOKit、peer descriptor) を指す。
- 通知対象の「状態」の正本は 9.1 Cluster mode の `ClusterState` / `StableMode` と、18.1〜18.6 の状態遷移フロー
  (Booting→SoloStandaloneReady、PairedStandalone、DistributedReady、Demoting、Backoff、ManualInterventionRequired)。
- したがって本機能は spec の target behavior を変えない付加レイヤとして設計する。

### 2.2 状態変更の観測点（現行コード）

| 観測点 | 場所 | 内容 |
|---|---|---|
| 全状態遷移 | `ClusterHandle::subscribe()` → `watch::Receiver<ClusterSnapshot>` | 状態機械がpublishする全遷移。`app.rs` の `spawn_transition_monitor` が既に購読してmetrics/persistを駆動 |
| 起動時 | `serve()` → `spawn_runtime` | `ModeRuntime::spawn_ready_at` (Booting→SoloStandaloneStarting→SoloStandaloneReady) または `spawn_manual_at` (ManualInterventionRequired)。transition monitor は起動後の値から購読開始するため、起動時の遷移はwatchでは拾えない |
| standalone再起動 | `spawn_local_monitor` | `child_restart("standalone", "unexpected-exit")` で検知 |
| 状態名 | `ClusterState::name()` / `StableMode::name()` / `LocalRole::name()` | リファクタリングで一元化済み。通知文言に再利用可能 |
| 遷移分類 | `state.rs::transition_name` | pair / promote / demote / reconcile の分類が一元化済み |

### 2.3 既存インフラ

- 通知系のインフラは存在しない。macOS依存crateは `libc` / `system-configuration` のみ。
- `/usr/bin/osascript` は標準搭載 (`terminal-notifier` は未導入)。
- 正規デプロイは LaunchAgent (`contrib/launchd/local.siderostat.runtime.plist`、`gui/$(id -u)` ドメイン)。

## 3. 通知手段の選定

| 方式 | 追加依存 | 評価 |
|---|---|---|
| `osascript` (`display notification`) | なし (/usr/bin標準) | **推奨**。プロセス生成コストはあるが、バンドル不要でLaunchAgentから動作し、文言・音 (`sound name`) を制御可能 |
| `mac-notification-sys` crate (UserNotifications framework) | あり (objc2) | ネイティブだが、bundle identifierが無いと表示元アプリ名が不自然になりがち。署名/バンドル調整が必要 |
| `terminal-notifier` | 要導入 | 追加バイナリ管理が必要。非推奨 |

→ **`osascript` 方式**を採用する。`tokio::process::Command` でspawnし、失敗はwarnログのみとする
(spec 18.5「Peer通知不能でもlocal recoveryを妨げない」という哲学と整合)。

## 4. 実装構成

### 4.1 新モジュール

`src/cluster/platform/notify.rs` (macOS実装) + 非macOSは no-op。

- `trait DesktopNotifier { fn notify(&self, title, body) -> BoxFuture<Result<(), NotifyError>>; }`
  - テストではfakeを注入可能にする。
- macOS実装: `osascript -e 'display notification "<body>" with title "<title>" sound name "Glass"'` をspawn。
- `DesktopNotificationService` (購読とイベント選定・throttleを持つ) を同じモジュールまたは `src/app.rs` に置く。

### 4.2 購読タスク

- `app.rs` に `spawn_desktop_notifier(...)` を追加し、`spawn_transition_monitor` と**並列に**
  `runtime.cluster_handle().subscribe()` を購読する。
- 起動時は `serve()` 側で明示的に1回通知する (`spawn_runtime` 直後に「起動完了」または「要手動対応」)。
  transition monitor は起動後の値から購読開始するため、起動遷移はwatchでは拾えない点に注意。
- standalone再起動は `spawn_local_monitor` の `child_restart` 検知に合わせて通知を発火する。

### 4.3 通知イベントの選定（全遷移ではなく重要イベントに絞る）

| カテゴリ | トリガー | 通知例 |
|---|---|---|
| 起動時 | serve開始 / 起動完了 / 起動失敗 | 「siderostat 起動」「SoloStandalone 起動完了」「要手動対応」 |
| standalone | `SoloStandaloneReady` / `PairedStandaloneReady` / child再起動 | 「Standalone 準備完了」「Standalone が再起動されました」 |
| distributed | `DistributedReady` / promotion失敗 (Backoff・ManualInterventionRequired) / demote完了 | 「Distributed MXFP4 準備完了」「プロモーション失敗・バックオフ」「Distributed 停止・Paired へ復帰」 |

- 遷移途中の `Promoting`→`DistributedStarting` などは通知せず、安定状態 (Ready/Backoff/ManualInterventionRequired) と
  重要遷移のみに絞る。
- **Throttle**: 高頻度遷移 (再起動ループ等) 対策に最短通知間隔 (例: 5秒) を設ける。

### 4.4 設定

`ModeAwareConfig` に新 `[notifications]` セクションを追加 (`#[serde(default)]`)。

```toml
[notifications]
enabled = true      # 既定値: macOS で true / 他プラットフォームは no-op
sound = true        # osascript の sound name 使用
```

`LoggingConfig` と同じ `#[serde(default)]` パターンで追加し、既存設定との後方互換を維持する。

## 5. 安全設計（詳細）

### 5.1 非ブロッキング・失敗耐性

- 通知は `tokio::process::Command` で非同期spawnし、状態機械・proxy・persist の処理を一切ブロックしない。
- 投稿失敗・`osascript` 不在・非GUIセッションは **warnログのみ** で、クラスタ動作に影響させない。

### 5.2 GUIセッション判定 (`launchctl managername`)

- 通知の直前に `launchctl managername` を実行し、出力が `Aqua` でなければスキップ＋warnログ。
- 追加依存なし・安価 (この環境では `launchctl managername` → `Aqua` を確認済み)。
- 起動時に1回だけ通知可否をログし、運用者がデプロイ形態の誤りに気づけるようにする。

### 5.3 launchd のセッションモデルと通知の関係

macOS の launchd はジョブをどのドメインに載せるかで実行コンテキストが決まる (`man launchctl`)。

| ドメイン | 対象 | 実行コンテキスト |
|---|---|---|
| `system/` | LaunchDaemon | 特権・root Mach bootstrap。ログイン前/ブート時から起動。ユーザーのGUIセッションとは別 |
| `user/<uid>/` | user agent | ログインと独立に存在できる (SSHセッション等) |
| `gui/<uid>/` (=`login/<asid>/`) | LaunchAgent | ユーザーがGUIログインしたとき作られる **Aquaセッション** |

- Notification Center (`usernoted`) はユーザーのAquaセッション内で動作する。
- `osascript display notification` は posting プロセスが「そのユーザーのAquaセッション」に属している必要がある。
- **LaunchAgent を `gui/<uid>` に載せた場合は通知が表示される**。正規デプロイはこの構成。
- **LaunchDaemon (`system/`) や `user/<uid>` ドメインの場合は通知不可** (エラーになるか、表示されない)。
- plist の `ProcessType = Background` はリソース制限 (CPU/IOスロットル) の分類であり (`man launchd.plist`)、
  セッション種別や通知可否には影響しない。
- 完全ログアウト時は `gui/<uid>` ドメイン自体が破棄され agent も停止するため通知は出ない (想定どおり)。
  画面ロック中は Notification Center がロック画面にも表示するため通知は出る。

### 5.4 plist への `LimitLoadToSessionType = Aqua` 明示

- `man launchd.plist` によると agent のみに効くキーで、非GUIドメインへのロードを防げる。
- 誤って `user/<uid>` に載せた場合にロード自体が失敗し、黙って通知が出ない事態を避けられる。
- contribのREADMEに「GUIドメイン必須」の検証手順 (`launchctl print "gui/$(id -u)/..."`) を追記する。

### 5.5 helper agent パターン（将来ブート時運用が必要な場合）

- systemドメインのdaemonから直接通知するのは不可能なため、
  daemonは状態変化をUnixソケット/ファイルで伝え、GUIセッション内のLaunchAgentが `osascript` で投稿する構成が正攻法。
- 現行仕様はLaunchAgent運用のため、本提案では必須としない。

## 6. テスト方針

- `DesktopNotifier` をトレイト化し、fakeを注入して「どの遷移でどのタイトル/本文がemitされるか」の単体テストを追加
  (遷移表ベース)。
- 非GUIセッション (fakeで `Aqua` 以外) で通知がスキップされ warnログになることのテスト。
- phase1〜5統合テストは通知を無効化して実行する (UI依存のためCIでは通知を出さない)。

## 7. 実施にあたっての指針

- 挙動を変えない付加レイヤとし、`docs/spec.md` の target behavior を変更しない。
- 回帰検証は Required CI (`cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` /
  `cargo test --all-targets`) と `tests/phase1`〜`phase5` 統合テストで確認する。
- Git運用は `CONTRIBUTING.md` に従い、1 commitを1つのreview可能な目的に限定し、実装と対応するtestを同じcommitへ含める。

## 8. リスク・注意点

1. **launchd からの通知**: `gui/<uid>` ドメインのLaunchAgentなら表示されるが、LaunchDaemon化・`user/<uid>` への
   誤載せでは表示されない。5.2〜5.4 の判定とplist明示で安全側に倒す。
2. **プロセス生成コスト**: 通知1件あたり数十ms。頻度が高い遷移はthrottleで吸収する。
3. **コード署名/バンドル**: ネイティブframework方式を採らないため不要。osascriptは表示元が「osascript」または
   実行ユーザーになる。
4. **TCC**: `display notification` は他アプリをApple eventsで操作しないためAutomation許可は通常不要。
   デーモンではその前の「セッション到達性」の段階で失敗する。
