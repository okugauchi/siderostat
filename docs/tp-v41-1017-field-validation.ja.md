# V4.1 Flash 1017 Q2 実機検証記録

確認日: 2026-10-27。M5 Max MacBook Pro / M4 Max Mac Studio、各128GB。
これは途中経過であり、通常runtimeのTP受入完了を示すものではない。

## 導入済みの条件

- 両ノード: siDeroStat 0.4.0 build33、source `a7208dd`。
- 上流ds4: `ee7e8405232c8971000cc7ba2b945b00bf6d20d3`、各ノードでnative build。
- モデル: [固定値](../examples/tp-v41-1017/model-lock.json)に一致する1017 Q2の通常ファイルを両ノードに配置。全体SHA-256検証済み。
- Studioがcoordinator、MBPがworker。制御はBridge `10.99.0.1/2`、RDMAはStudio en5 `10.99.10.1/30`、MBP en2 `10.99.10.2/30`、GID index 2。
- Context 8192、同時要求1、Vision/DSparkなし。StandaloneはSSD streaming、expert cache 16GB。
- 両ノードの `iogpu.wired_limit_mb=98304`。expert shard等の計画量は各83.88 GiB。
- en5/en2のIPv4 aliasとwired limitは再起動後に再設定が必要。永続化は未実施。
- Local Networkの許可を両ノードで確認。

## 確認できたことと未完了のこと

Standaloneは両ノードで公開APIの非stream生成とSSE生成に成功。直接起動した上流TPも、実RDMA接続と生成に成功した。
ただし通常build33はRDMA/HTTP準備後、内部ウォームアップ中に `TP peer lost during warm-up` で復帰した。
以前の通常TPにも生成中のHTTP 500があり、通常runtimeによる安定運用の受入は未完了。
現時点の運用policyはForcedStandaloneへ戻し、失敗したTPの自動再試行を停止した。

## バックグラウンド実行の比較

同一モデル・native binary・context・RDMA選択・wired limitを用い、通常runtimeを停止して上流を直接起動した。
バックグラウンド指定はOS標準の `taskpolicy -b` を使った。推論要求は逐次送信した。

| 条件 | 内部ウォームアップ相当: 33 prompt tokens / 1 completion token | 続く短い生成 |
|---|---|---|
| `taskpolicy -b`付き | HTTP 500、37.482秒、`tp: prefill synchronization failed` | 障害後の要求は送信せず停止 |
| バックグラウンド指定なし | HTTP 200、24.799秒 | HTTP 200、`OK`、0.348秒 |

失敗時のworker stderrには `big gate header exchange failed or peer closed (layer 4 seq 10): Resource temporarily unavailable`、
`TP gate exchange failed` があり、coordinatorにも対応するgate失敗があった。上流のgate timeoutは750ms。
以前のバックグラウンド指定なしの直接試験でも、同じ96 GiB条件でウォームアップ10.727秒、短い生成0.306秒に成功した。

この比較はバックグラウンド制限が障害要因であることを支持する。キャッシュや初回初期化時間を完全に統制した速度比較ではなく、
実LaunchAgentの修復確認とも区別する。

## 修正候補と次の受入

配布Appのruntime LaunchAgentは `ProcessType=Background` だった。
ユーザーから要求された推論APIとその子プロセスに応答性が必要なため、`Interactive`へ変更する。
`Adaptive`の切替条件となるXPC transactionはこのHTTPサービスでは管理していない。
実行優先度の変更はLP/Standaloneにも及び、負荷時のCPU/I/O競合が増える可能性がある。
モデル、RDMA timeout、試行上限、制御認証、受付制御は変更しない。

変更した署名済みAppを両ノードへ導入し、実際のLaunchAgentの設定値、通常TPの内部ウォームアップ、
両公開APIへの逐次生成とSSE、同一sessionの維持を確認する。切断・再接続と再起動の受入はその後に実施する。

## 診断出力を取得する際の注意

このmacOSでは `launchctl debug --stdout <path> --stderr <path>` のpathが起動引数へ混入する挙動を実測した。
`--`で実行ファイルを明示するだけでは防げなかった。ログ転送を行う場合は、引数なしの `--stdout --stderr` と
shellの出力リダイレクトを使用し、再起動前に `launchctl print` のcontroller-supplied argumentsを確認する。
`launchctl debug`自体は管理者権限が必要で、ログ転送中は待機したままになる。
設定が競合した場合は重ねて登録せず、通常LaunchAgentを復元してから設定し直す。

runtimeは上流TPの生ログを保存しないため、runtime出力だけではnativeのgate失敗の詳細を取得できない。
今回の詳細は隔離した直接起動試験から得た。promptやsecretを含む生出力の恒久保存を有効にする変更はしていない。
