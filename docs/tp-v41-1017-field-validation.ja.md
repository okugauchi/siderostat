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

## build34の通常導入後

source `dd2d8fd`、build34の署名済みpkgを両ノードへ導入。
署名、Siderostat.appだけのpayload、生成物検証、1,061件の回帰試験に成功した。
両ノードの実LaunchAgentが `spawn type = interactive (4)` で動作することを確認。
最初のAutomatic適用は制御接続エラーとなったが、制御listenerとTCP到達性を確認後の再試行に成功した。

通常TP session 7は内部warm-upを通過し、両ノードの公開APIで非stream/SSEの短い生成に成功。
短い非stream応答はMBP 0.335秒、Studio 0.326秒、同一TP sessionを維持した。
しかし、その後の「1から20まで数える」要求（max_tokens 128）でHTTP 500が再発した。
**Interactive化だけでは安定運用の受入は完了していない。** 自動再試行を止め、ForcedStandaloneへ戻した。

## 通常750msと診断用5000msの追加比較

通常750msの上流直接TPで、warm-up、短い生成、39トークン生成、待機後の39トークン生成には成功。
続くSSE要求がHTTP 500となり、両rankにRDMA gate timeoutとprefill同期失敗を確認した。
workerは `seq 9681 (recv_done 9680)`、coordinatorは `seq 9682 (recv_done 9681)`。
この差だけでpacket lossとは断定できず、先に片側が期限切れとなる処理遅延でも説明可能。

既存の診断用 `DS4_TP_GATE_TIMEOUT_MS=5000` を直接起動の子プロセスだけへ指定し、
warm-up、短い生成、39トークン生成、30秒待機後の生成、10秒待機を挟むSSE/39トークン生成6組、
合計16要求がすべてHTTP 200となった。両rankの実ログで `gate-timeout=5000ms` を確認。
この値は要求全体の制限ではなく、big-prefillの一部は値+2000msとなる。

上流の `QA_BEFORE_RELEASES.md` はこの変数を診断overrideと位置づけ、正常推論に必要な設定とは扱っていない。
したがってAppや恒久設定へ反映せず、診断childを停止した。通常manifestの一致は、この環境変数の一致を保証しない。
ユーザーは暫定延長の採用より、上流側の原因調査と通常750msでの安定化を優先する方針を選択した。
上流mainの最新も `ee7e840` であることを確認済み。

## 2026-10-28: main queue keepaliveの比較とmanaged TP対応

詳細traceでは、通常gateの両ノード差の中央値9.285msに対し、idle後の該当gateは738.553msだった。
差を補正すると約729msの到着差であり、CPU交換は750ms以内に進む一方、GPU pollが先に枯渇したケースを確認した。
pollを共有event待ちへ切り替えただけでも、別試行でStudioのGPU開始に約1.4秒の遅れがあり、CPU期限を超えた。
したがってpollの待機方法だけを変更する対処は採用していない。

元ソースと別の診断cloneで、keepalive投入・完了と遅いCBのdriver時間だけを追加記録した。
main queueの投入間隔は既定1000msに対して約1.0〜1.2秒で、TP専用keepaliveも約15msで正常完了していた。
それでもmain queueの小さいCB自体に、driver内で1秒以上かかる場面があった。

Studioでstartupを除き、推論の開始〜終了の前後1秒にも重ならないidle sampleを比較した。

| `DS4_METAL_QUEUE_KEEPALIVE_MS` | idle sample数 | 100ms超 | 最大 | 中央値 |
|---|---:|---:|---:|---:|
| 既定1000 | 193 | 112 | 1465.9ms | 180.8ms |
| 100 | 878 | 0 | 0.3ms | 0.1ms |

診断版は配布せず、元の上流binaryに戻し、keepalive100msだけを指定して再検証した。
両roleのSHA-256が既存の承認済みrole manifestと一致することを再確認した。
750ms・通常pollを維持し、warm-up、非stream、30秒idle、10秒idleを挟むSSE/39トークン生成12組の
28要求がすべてHTTP 200となった。さらに39トークンのSSEも、41eventsと`[DONE]`、数列の正しさを確認した。
そのSSEは最初のcontentまで0.537秒、全体1.831秒だった。

build35では、TP両roleのchild起動だけに100msを指定し、存在だけで無効化する
`DS4_METAL_DISABLE_QUEUE_KEEPALIVE`をchild環境から除去する。親・Standalone・LPの環境は変更しない。
nativeのsource/binary/help/modelとmanifest schema、gate timeoutは変更しない。
電力影響ゼロや全モデルでの安定性、GPU pollの全ての安全性問題を解消したとは主張しない。
診断用5000msを採用したものでもない。通常runtimeのbuild35受入結果は後述する。

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

## 2026-10-28: build35の通常runtime受入

両ノードへsource `7b922c5`、version 0.4.0/build35を導入した。
pkg SHA-256は `a1db3b3471692f671af81a9cd4f4636bbb79fa5d8d9125ebfa94a530e4e8b058`。
Developer ID署名済み・timestamp/公証なしの手動検証用pkgであり、正式公証済み配布物ではない。
両LaunchAgentはInteractive、native binaryは既存role manifestと一致したまま。
実プロセスから限定した環境変数だけを検査し、TP両roleの100ms適用、disable変数とgate overrideの不在、
親runtimeとStandaloneへの非適用を確認した。従って通常750msを維持している。

### 生成、待機、モード変更

- 導入直後のStandalone: 両公開APIで非streamとSSEに成功。
- Automatic epoch 11 / TP session 9: 両公開APIで非streamとSSEに成功。
- session 9の39トークン生成: 非stream、両公開APIのSSE、30秒idle後の非streamに成功。
  SSEはそれぞれ41eventsと`[DONE]`、数列1〜20の一致を確認し、最初のcontentは0.524/0.542秒。
  30秒idle後の非streamは1.881秒だった。
- ForcedStandalone epoch 12: 両TP childが停止し、両公開APIで非streamとSSEに成功。
- Automatic epoch 13 / TP session 10: 手動pairや再試行なしで再形成し、両公開APIで非streamとSSEに成功。

### 生成中のworker runtime再起動

Studio側で1〜200の数列をSSE生成し、最初のcontentを受信した後、MBPの専用runtime LaunchAgentへSIGTERMを送った。
この要求は280events、途中448文字、`finish_reason=error`で終了し、完全な数列の成功として扱われなかった。
`[DONE]`も届いたため、SSEの`[DONE]`だけでは生成成功と判定できない。
終了対象のruntimeとTP worker childの残留はなく、launchdは新しいruntimeを自動起動した。
StudioはAutomatic epoch 13のままSolo Standaloneへ移行し、非streamとSSEに成功した。
Studioの永続化されたTP失敗回数は1、失敗sessionは10であり、試験のためのjournal書換えや試行枠の初期化は行っていない。
再起動したMBPは365 GBのモデル全体のSHA-256検証を行うため、その間APIの待受開始も待機する。
モデル検証後、policy epochを変えずにTP session 11へ自動復帰した。復帰後も両公開APIの非stream/SSEに成功し、
Studioの39トークンSSEは41events・数列一致・`[DONE]`、最初のcontent 0.521秒、全体1.849秒だった。
再生成された両TP childにも100msだけが適用され、gate overrideとdisable変数は不在だった。
最終状態は両ノードともTP Ready・serving、in-flight 0。Studioの失敗回数1/session 10は保持された。
通常750msでの今回の生成・待機・モード往復・worker runtime再起動の受入は成功した。


### 条件と未実施項目

今回の構成はM5 Max / M4 Max各128 GiB、1017 Q2、context 8192、resident TP、同時要求1。
RDMA memberはMBP en2/rdma_en2・10.99.10.2、Studio en5/rdma_en5・10.99.10.1、両GID2。
`iogpu.wired_limit_mb=98304` とen2/en5の10.99.10.x/30 aliasは再起動で失われる設定なので、OS再起動後に再設定・再確認する。
物理ケーブルの抜去・再接続、OS全体の再起動、長時間連続運転はこの試験では未実施。
100msによる電力影響と他モデル・他contextでの安定性は未評価。GPU poll自体の一般的な安全性を保証する結果ではない。
詳細証跡はローカルのignore対象 `dist/tp-build35/evidence/` に保存する。

## 2026-11-01: アイドル中の物理ケーブル切断

build35、TP session 12、in-flight 0でThunderboltケーブルを抜いた。両ノードでruntimeが終了し、
管理APIに接続できず、切断中のStandalone動作は確認できなかった。再接続後も孤立したTP childが残り、
両runtimeは起動を繰り返した。記録済みPID・実行ファイル・起動時刻と一致し、親がinitとなっていた
TP childにSIGTERMを送り、終了確認後に両runtimeが立ち上がってTP session 13を形成した。
両APIの1〜20の非stream/SSE生成に成功したが、これは手動回収後の復帰である。

macOSログでは01:00:52.559にMBP側bridge0のIPが除去され、01:00:53.167にruntimeがexit(0)した。
role change monitorは役割変化時に`std::process::exit(0)`を呼び、所有childの通常停止を通さない。
時刻と孤立childはこの経路と整合するが、runtime自体の終了ログは取得できなかった。
両ノードの`startup_cleanup.auto_restart=false`を確認した。再起動失敗の直接原因は未確定。

修正候補はrole変更とrole変更を伴うadmin restartを、既存server loopの終了通知へ接続する。
受付停止・listener終了・所有child停止を既存経路に任せ、環境設定や無関係なprocessを変更しない。
回帰試験は旧版で即時exitにより失敗し、修正版で通知が届きprocessが生存することを確認する。
接続断中や生成中の排出・停止および実機受入は別途必要であり、この試験だけで保証しない。

### build36の切断後の起動失敗と修正

build36では停止中の表示とTP childの回収を確認したが、切断中のStandalone復帰には至らなかった。
両ノードでbridge0のIPv4消失後、役割不明の起動時にTP共通検証がモデル全体を読み、
最後に`TP role unknown`で終了する経路を隔離fixtureで再現した。実機の終了時stderrは取得できていない。

修正後は役割不明でもモデルとcapabilityの共通検証、両役割の承認情報を検証し、
役割別のファイルと実行ファイルの検証だけを後回しにする。役割不明でのTP起動は引き続き拒否する。
再接続後は既存の役割変更による再起動で、確定した役割の実行ファイルを検証する。
モデル全体のSHA-256検証は引き続き必要なため、Standaloneの起動にも時間がかかる。
### build37のアイドル中切断・再接続

build37、TP session 16、in-flight 0でケーブルを抜去した。両TP childの終了を確認し、
両runtimeは正常終了後に再起動した。記録上、MBPは12:33:00、Studioは12:33:35に
Solo Standalone Readyに到達し、切断中に各公開APIの短い非stream生成で`OK`を受信した。
12:34頃の再接続後、両ノードは12:38:33にTP Readyへ自動復帰し、session 17で
MBP公開APIの短い非stream生成に成功した。時刻は日本時間、観測はpoll時刻である。
状態記録はローカルのignore対象 `dist/tp-build37-evidence/cable-idle-states.jsonl` に保存した。
反復切断および長時間安定性は今回未検証。

### build37の生成中切断・再接続

TP session 17でストリーミング生成を開始し、推論内容の出力中にケーブルを抜去した。
出力を820チャンク受信した後、明示的なerror、finish_reason、`[DONE]`を受信しないまま
接続が閉じ、クライアントはcurl error 18を報告した。途中出力は生成成功ではない。
両TP childの終了後、Studioは12:44:35、MBPは12:45:14にSolo Standalone Readyに到達し、
各公開APIの短い非stream生成が成功した。再接続後は両ノードが12:50:16までにTP Readyに
復帰し、MBP公開APIで短い非stream生成に成功した。時刻は日本時間、観測はpoll時刻である。
切断時のstreamを明示的な失敗イベントで終えることは未達であり、別途原因調査が必要。
状態とstreamの記録は `dist/tp-build37-evidence/cable-active-states.jsonl` と
`dist/tp-build37-evidence/cable-active-response.sse` に保存した。

後続のソース修正では、役割変更による再起動時に待機中のstreamを中断し、SSE接続が
利用できる場合は `stream_interrupted` error eventを返す。応答開始前の要求は503で終える。
非圧縮の生成API SSEはイベント単位で最大1 MiBを保持し、未完了イベントを送る前に切断された
場合はその部分を破棄してerror eventを返す。上流接続エラー・stream idle timeout・`[DONE]`
のないEOFも失敗とし、成功を示す `[DONE]` は追加しない。圧縮SSEは平文イベントを混入させず
従来どおり通信エラーとして終了する。通常のSIGTERMとlistener異常による停止はこの中断経路の
対象外であり、長時間要求とOSの停止期限の組み合わせは別途検証が必要。
HTTP応答開始後はステータスを変更できず、クライアントへの接続自体が失われた場合はイベントの
到達を保証できない。この後続修正は実機で未検証。
