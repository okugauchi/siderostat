# H06 Manager GUI Design

## 目的

H06 の GUI gate を満たすため、現在のメニューバーアプリから DS4 Manager の操作画面を開けるようにする。画面から公式 source の取得、隔離 build、model download、verify、stage、activate、rollback を実行し、各 job の進捗・失敗理由・旧 active artifact を確認できる状態を作る。

既存の runtime、登録状態、model、secret、manifest は自動削除しない。実機受入では、既存 v0.3.4 backup と v0.4.0 の rollback 候補を保持したまま操作する。

## 適用範囲

### 含めるもの

- メニューバーに「管理画面を開く」を追加する。
- AppKit の管理ウィンドウを1プロセス内で1つだけ保持し、再度開いたときは既存ウィンドウを前面化する。
- `ManagerViewModel` と `ModelView` をウィンドウの状態源として使い、ウィンドウを閉じても job 状態を破棄しない。
- `MetricsClient` を介して manager API を worker thread から呼び、AppKit main loop をネットワーク待ちで停止させない。
- source/build/download/verify/stage/activate/rollback の開始、cancel、status polling、redacted error 表示を画面へ接続する。
- activation/rollback は runtime の現在 generation と lease を取得できるときだけ有効にする。条件不足時は disabled と理由を表示する。
- 現在の manifest/catalog から MXFP4/0731、Q2、Vision、GLM、prefix-file の対応情報を読み、checksum・encoder・support・prefix-file の検証状態を表示する。

### 含めないもの

- 重い model の再取得、既存 model の削除、既存 runtime の自動停止。
- 管理画面からの sudo install、SMAppService の無確認変更、外部 provider や Docker の導入。
- manager domain の build/download/verify/stage/activation ロジックをGUI側へ複製すること。GUIは既存 `/manager/*` 契約を呼び出し、runtime側のexecutor結果を表示する。現行runtimeの `JobJournal` がsubmit後にterminal更新を行わない場合は、GUI実装だけでH06をPASSへ変更せず、runtime executor bridgeを別の未達として記録する。
- 画面を別の常駐アプリへ分離すること。二重メニューバーアイコンの再発を避ける。

## アーキテクチャ

### Main thread

`main.rs` が `NSApplication` と `MonitorTray` を作成した後、`ManagerWindowHost` を1つ生成して保持する。host は AppKit window、表示用の labels/buttons、現在の画面状態を所有する。メニューバーの open イベントは host の `show_or_focus()` を呼ぶだけにし、window 作成を複数回行わない。

AppKit のコントロールイベントは `ManagerCommand` として command channel へ送る。main thread はコマンド投入と画面更新だけを担当し、HTTP、JSON parse、job polling、redaction は worker thread で行う。

### Worker thread

worker は `MetricsClient` の clone を使い、`ManagerApi` の concrete adapter を実装する。adapter は `submit_manager_job`、activation/rollback の generation・lease付き submit、`cancel_manager_job`、`fetch_manager_jobs` を呼び出す。結果は `ManagerEvent` として main thread の event queue へ返す。

`ManagerViewModel` / `ModelView` は event を適用する純粋な状態源として残す。view model の既存 fake 境界は維持し、実 adapter のテストでは HTTP fixture を使う。

### 画面構成

ウィンドウは次のセクションを持つ。

1. **Runtime / source**: runtime version、active digest、source commit、公式 source 取得、ds4-server build。
2. **Artifact pipeline**: download、verify、stage の対象と進捗。失敗時は `redact_secrets` 後の理由だけを表示する。
3. **Profiles**: MXFP4/0731、Q2、Vision、GLM、prefix-file の compatibility 状態。checksum欠落、encoder不整合、support不整合、prefix-file差分は activate disabled と理由表示にする。
4. **Activation / rollback**: activate と previous artifact の rollback。承認は activation 操作単位で一度だけ行い、各stageで重複ダイアログを出さない。
5. **Jobs**: running/cancelling/succeeded/failed の一覧、progress、cancel、再読込。

操作対象が未準備の artifact は「未準備」と表示し、該当 capability の smoke を pending とする。未準備を成功扱いにはしない。

## 状態遷移と安全性

- window close は host と worker を終了させず、job polling を継続する。再表示時に最新 status を反映する。
- build/download 中は旧 active digest を表示し続ける。
- submit が失敗した場合は active artifact、registration、runtime を変更しない。
- new startup/readiness が失敗した場合は activation API の rollback 結果を待ち、旧 artifact を表示へ戻す。rollback 自体が失敗した場合は manual intervention と表示する。
- upgrade/registration 操作は既存 backup と identity を検証してから開始する。GUIからの失敗注入は小 fixtureで行い、実機では v0.3.4 backupを保持する。
- 秘密情報、bearer token、URL userinfo、query token、raw build log は画面とログへ出さない。

## H06判定に関する既知の境界

現行のruntime `/manager/jobs` は `JobJournal` への登録と cancel を提供し、submit後の実処理を別workerへ接続していない。したがって本設計でGUIを配線しても、runtime側が terminal job を返さない限り、実機の「成功/失敗build」「download中断」「upgrade/rollback復元」のH06 gateは完了しない。この境界をGUIの成功表示で隠さず、terminal更新が観測できない場合は `pending（runtime executor未接続）` と記録する。

実機H06をPASSへ進める条件は、GUI配線に加えて、既存manager domain関数を呼ぶruntime executor bridge（または同等の実処理接続）が存在し、job statusがrunningからsucceeded/failedへ遷移することをfixtureと実機で確認できることである。

## 起動と重複防止

- bundle mode では既存 `NSApplicationActivationPolicy::Accessory` と AppKit main loop を使う。
- app-dev bundle を別途起動したままにしない。受入前に `/Applications/Siderostat.app` と開発ビルドのプロセスを確認し、正式版だけを残す。
- Manager window はアプリ内の単一 host とし、二つ目の Siderostat process を起動しない。
- AppKit 初期化失敗は無言で終了させず、logへ原因を残し、trayが生きている場合は操作行へ redacted error を表示する。

## テストと受入

### 自動テスト

- manager window host の open/focus、close後job保持、command dispatch を test-support で検証する。
- concrete adapter の fetch/build/download/verify/stage/activate/rollback と cancel を HTTP fixture で検証する。
- generation/lease不足、checksum不足、Vision/encoder不整合、prefix-file不一致、redacted error を negative test として先に実行する。
- 既存 H02/H03 の runtime health、minimal request、TP smoke、既存 model を変更していないことを再確認する。

### 実機GUI受入

1. 正式版メニューバーから管理画面を開き、source取得からstageまでを小fixtureで実行する。
2. 失敗build、download中断、checksum不一致 artifact を注入し、旧 active と登録状態が維持されることを確認する。
3. v0.3.4 backupを保持した状態で upgrade失敗→registration復元を確認する。
4. new startup失敗→旧artifact復帰、rollback後の health/ready を確認する。
5. MXFP4/0731、Q2、Vision、GLM、prefix-file の表示と最小request互換を確認する。artifact未準備は pending と記録する。

## 変更対象

- `monitor/src/main.rs`: host生成、manager menu event、worker event polling の配線。
- `monitor/src/tray.rs`: 管理画面を開く menu item と event helper。
- `monitor/src/manager_window.rs`: AppKit host、concrete adapter境界、画面状態の接続。既存 view model の責務は維持する。
- `monitor/src/client.rs`: activation/rollback context付き manager submit と必要なread-only status helper。
- `monitor/tests/`: window host、adapter、negative/fixture受入テスト。
- `docs/superpowers/plans/`: 承認済み設計に対応する実装計画。
- `Vault: evidence/H06.md`: GUI実機gateのcase別証跡。未実行ケースをPASSへ変更しない。

## 代替案を採用しない理由

- tray submenuだけでは、H06が要求するManager画面、job継続、profile表示を確認しにくい。
- 別アプリ方式は常駐プロセスとメニューバーアイコンを増やし、今回解消した二重起動問題を再発させる。
