# 停止進捗の表示

## 目的と制約
役割変更・停止で受付を止めた後、実際の要求排出と子プロセス停止の状態を同一ノードのMonitorへ表示する。通信不能だけから停止・回収成功を推定しない。TP復旧修正の未コミット変更を保持する。

## 方式
runtimeの監視APIを子プロセス停止まで維持し、受付停止中は管理操作の変更要求を拒否する。フェーズは実処理開始時と成功/失敗時に更新し、識別情報やpromptを含めない。workerでもローカルの停止フェーズを取得する。API消失時はoffline/状態不明とし、古い成功を保持しない。

## 実行
1. APIとMonitorの表示、停止途中の新規管理操作拒否、通信断・旧runtime・停止失敗の試験を先に作り失敗を確認する。
2. runtime側の終了順序と状態公開、Monitorのローカル状態取得とメニューバー表示を接続する。
3. drain中の長時間要求、listener失敗、TP child停止失敗、同期/競合、runtime強制終了、workerのcoordinator metrics参照を確認する。
4. フォーマット・clippy・全テスト・レビューを実施し、実機確認が済むまでは受入未完了と記録する。

## 実装上の判断と残る制限

- 役割変更と手動再起動は終了ownerへ通知し、local metricsでdraining / stopping-child / failedを公開する。workerはまずlocal metricsを確認する。
- 停止前に受理した管理操作をゲートとjob記録で待ち、タイムアウトまたはchild停止失敗ではownerを保持し明示retryを待つ。admin listener自体が失われた場合は同じownerが間隔を置いて再試行する。
- 短い段階は監視間隔により表示されない場合がある。API喪失時はofflineであり回収完了とは断定しない。
- 公開要求の終了待ちは現状無期限。実機のLaunchAgent停止期限5秒は未変更であり、外部のサービス停止時に長いdrainやchild停止が強制中断される可能性がある。役割変更による自発shutdownとは区別し、実機受入前の未解決制限とする。
- 前回のbuild35実機試験では失敗後のadmin終了理由は直接取得できていない。新実装の実ケーブル・生成中・OS停止受入は未実施。

## 検証結果

- 2026-11-01: `cargo fmt --check`、workspace全target/all-feature clippy（`-D warnings`）、全test 68 suites / 1,085件 / 失敗0。
- 実HTTP listenerで子停止処理中のmetrics応答と停止後のlistener終了を検証。workerのlocal優先、旧runtime互換、通信断後の表示、管理操作の排他、timeout・停止失敗・retry競合も検証。
- 独立レビューで検出した競合を修正し再確認した。署名済み成果物の再作成・両ノードへの導入および実機試験は未実施。
