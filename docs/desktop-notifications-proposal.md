# siderostat デスクトップ通知 実装提案

- 文書状態: 実装済み (2026-08-22 更新)
- 作成日: 2026-08-12
- 対象baseline: `develop` (`10ddf68` / リファクタリング完了後)
- 前提: 通常の状態通知は付加レイヤだが、startup cleanupの既存process通知は再起動操作の既定動作を定義する。

モード名と状態名の正規化は [`ds4-mode-taxonomy.md`](ds4-mode-taxonomy.md) に従う。

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

- 通知系の既存インフラは `src/notify.rs` に集約する。macOS のネイティブ通知には
  `objc2-user-notifications` を使用する。
- Runtime は `Contents/Helpers` の実行ファイルであり、単独の app bundle ではない。
  UserNotifications は署名済み `Siderostat.app` の Monitor から呼び出す。
- 正規デプロイは LaunchAgent (`contrib/launchd/local.siderostat.runtime.plist`、`gui/$(id -u)` ドメイン)。

## 3. 通知手段の選定

| 方式 | 追加依存 | 評価 |
|---|---|---|
| `UNUserNotificationCenter` (UserNotifications framework) | あり (`objc2-user-notifications`) | **採用**。署名済み Siderostat.app から呼び出すため、通知元と「表示」の対象を Siderostat にできる |
| `osascript` (`display notification`) | なし (/usr/bin標準) | 不採用。通知元が Script Editor になり、標準の「表示」で Script Editor が起動する |
| `terminal-notifier` | 要導入 | 追加バイナリ管理が必要。非推奨 |

→ **UserNotifications 方式**を採用する。Runtime は per-user Unix socket
(`~/Library/Application Support/siderostat/notifications.sock`) へ通知 payload を送り、
Monitor が native API で投稿する。relay または native 投稿の失敗は warn ログのみとし、
cluster の recovery を妨げない (spec 18.5 の哲学と整合)。

## 4. 実装構成

### 4.1 新モジュール

`src/notify.rs` の macOS 実装 + 非macOSは no-op。

- `trait DesktopNotifier { fn notify(&self, title, body) -> BoxFuture<Result<(), NotifyError>>; }`
  - テストではfakeを注入可能にする。
- macOS実装: Monitor の `UNUserNotificationCenter` へ `UNNotificationRequest` を登録する。
- Runtime は Monitor の relay socket に JSON payload を送る。socket は per-user application
  support directory に作成し、Monitor 起動時に stale socket を整理する。
- `DesktopNotificationService` (購読とイベント選定・throttleを持つ) を同じモジュールまたは `src/app.rs` に置く。

### 4.2 購読タスク

- `app.rs` に `spawn_desktop_notifier(...)` を追加し、`spawn_transition_monitor` と**並列に**
  `runtime.cluster_handle().subscribe()` を購読する。
- 起動時は `serve()` 側で明示的に1回通知する (`spawn_runtime` 直後に「ds4-server が Standalone モードで起動しました」または
  「ds4-server のモード変更に失敗しました (要手動復旧)」)。
  transition monitor は起動後の値から購読開始するため、起動遷移はwatchでは拾えない点に注意。
- standalone再起動は `spawn_local_monitor` の `child_restart` 検知に合わせて通知を発火する。

### 4.3 通知イベントの選定（全遷移ではなく重要イベントに絞る）

| カテゴリ | トリガー | 通知例 |
|---|---|---|
| 起動時 | serve開始 / 起動完了 / 起動失敗 | 「ds4-server が Standalone モードで起動しました」「ds4-server のモード変更に失敗しました (要手動復旧)」 |
| standalone | `SoloStandaloneReady` / `PairedStandaloneReady` / child再起動 | 「ds4-server が Standalone モードで起動しました」「ネットワーク上の ds4-server ノードを検出しました」「ds4-server を Standalone モードで再起動しました」 |
| Distributed (layer-parallel) | `DistributedReady` / promotion失敗 (Backoff・ManualInterventionRequired) / demote完了 | 「2つの ds4-server を Distributed（layer-parallel）モードに切り替えました」「ds4-server の Distributed（layer-parallel）起動に失敗しました。Standalone モードで待機します」「ds4-server のモード変更に失敗しました (要手動復旧)」 |

- 遷移途中の `Promoting`→`DistributedStarting` などは通知せず、安定状態 (Ready/Backoff/ManualInterventionRequired) と
  重要遷移のみに絞る。
- **Throttle**: 高頻度遷移 (再起動ループ等) 対策に最短通知間隔 (例: 5秒) を設ける。

### 4.4 設定

`ModeAwareConfig` に新 `[notifications]` セクションを追加 (`#[serde(default)]`)。

```toml
[notifications]
enabled = true      # 既定値: macOS で true / 他プラットフォームは no-op
sound = true        # macOS 標準通知音 (Glass) を使用
```

`LoggingConfig` と同じ `#[serde(default)]` パターンで追加し、既存設定との後方互換を維持する。

### 4.5 startup cleanup の通知

起動時に既存の `siderostat` / `ds4-server` process を検出した場合は、長文の確認ダイアログを表示せず、
UserNotifications で右上バナーと `Glass` 警告音を投稿する。通知本文は「5秒後に ds4-server を再起動します」
という簡潔な内容とし、5秒後に既存processをidentity再確認付きで停止して新しいsiderostatを起動する。
既定動作を拒否したい場合は `[startup_cleanup] auto_restart = false` または
`siderostat serve --decline-startup-cleanup` を使用する。通知の「表示」は Siderostat を対象とするが、
macOS の UserNotifications 公開 API には標準アクションボタンを常に非表示にする設定がないため、
拒否を画面上のボタンではなく明示的な設定/CLIオプションとして提供する。

## 5. 安全設計（詳細）

### 5.1 非ブロッキング・失敗耐性

- Runtime から relay socket への送信は非同期で行い、状態機械・proxy・persist の処理を一切ブロックしない。
- 投稿失敗・relay 不在・UserNotifications の拒否・非GUIセッションは **warnログのみ** で、
  クラスタ動作に影響させない。

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
- UserNotifications を呼ぶ Monitor はユーザーの Aqua セッションで起動する必要がある。
- **LaunchAgent / Login Item を `gui/<uid>` に載せた場合は通知が表示される**。正規デプロイはこの構成。
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
  daemonは状態変化をUnixソケット/ファイルで伝え、GUIセッション内のLaunchAgentが
  UserNotifications で投稿する構成が正攻法である。現行実装はこの relay パターンを採用する。
- 現行仕様はLaunchAgent運用のため、本提案では必須としない。

## 6. テスト方針

- `DesktopNotifier` をトレイト化し、fakeを注入して「どの遷移でどのタイトル/本文がemitされるか」の単体テストを追加
  (遷移表ベース)。
- 非GUIセッション (fakeで `Aqua` 以外) で通知がスキップされ warnログになることのテスト。
- phase1〜5統合テストは通知を無効化して実行する (UI依存のためCIでは通知を出さない)。

## 7. 実施にあたっての指針

- 状態遷移の通知は付加レイヤとして扱う。startup cleanupの5秒既定動作と拒否オプションは `docs/spec.md` に定義する。
- 回帰検証は Required CI (`cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` /
  `cargo test --all-targets`) と `tests/phase1`〜`phase5` 統合テストで確認する。
- Git運用は `CONTRIBUTING.md` に従い、1 commitを1つのreview可能な目的に限定し、実装と対応するtestを同じcommitへ含める。

## 8. リスク・注意点

1. **launchd からの通知**: `gui/<uid>` ドメインのLaunchAgentなら表示されるが、LaunchDaemon化・`user/<uid>` への
   誤載せでは表示されない。5.2〜5.4 の判定とplist明示で安全側に倒す。
2. **プロセス生成コスト**: 通知1件あたり数十ms。頻度が高い遷移はthrottleで吸収する。
3. **コード署名/バンドル**: UserNotifications は app bundle の識別子を必要とするため、
   Runtime から直接呼ばず、署名済み Siderostat.app の Monitor から投稿する。
4. **通知許可**: 初回の UserNotifications 投稿時に macOS の通知許可が必要になる。拒否された場合も
   runtime の状態機械は継続し、warn ログだけを残す。
5. **標準アクション**: 通知の「表示」クリックは Siderostat を対象にする。標準アクションボタン自体を
   常に隠す公開設定はないため、非表示を要件にしない。
