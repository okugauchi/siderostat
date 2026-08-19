# Hermes cron / DS4 分散推論スループット低下 調査報告

- 文書状態: 調査時点のスナップショットおよび改善提案
- 障害発生日: 2026-08-19 (JST)
- 調査基準日: 2026-08-19 (JST)
- 対象 Siderostat: commit `259b575c2b046bc47859bc84f0b6242ea72e02a4`
- coordinator: Mac Studio (`10.99.0.1`)
- worker: MacBook Pro (`10.99.0.2`)

## 1. 目的

Hermes Agent の三つの cron ジョブが、ローカルの DeepSeek V4 Flash を使用した際に
連続して失敗した事象について、Hermes、Siderostat、DS4、worker、および macOS のログを
突き合わせた結果を記録する。

本書は、確認できた事実と未確定の推測を分離し、原因が再現できない段階でも cron の
連続失敗を抑止できる運用回避策と、Siderostat に追加する自動復旧機能を提案する。

## 2. 結論

1. cron 失敗の直接原因は、DS4 の decode が通常の約 26--28 tokens/s から約
   0.16--0.18 tokens/s へ低下し、Hermes の非 streaming 呼び出しが 600 秒間応答を
   完了できなかったことである。
2. Hermes は同じ stale timeout を5回連続で検出し、無期限停止を避ける circuit breaker
   により `RuntimeError` を返した。この挙動自体は設定どおりである。
3. worker のスリープ、Thunderbolt bridge の切断、継続的な再送増加、持続的な thermal
   throttling は、確認したログでは認められなかった。Hermes の初期報告にあった
   「worker の深夜スリープまたはネットワーク劣化が強く疑われる」という評価は支持されない。
4. macOS セキュリティアップデートのダウンロードとインストールは、すべての cron 失敗後に
   開始された。アップデート負荷は障害原因ではない。ただし復旧時にはOS再起動と
   26.6.1から26.6.2への更新が同時に行われたため、復旧が単なる状態初期化によるものか、
   OS更新の効果を含むものかは分離できない。
5. 02:36ごろに開始された約48,000 tokenのcontextを持つ呼び出しを境に低速状態が現れ、
   その後の約29,000 tokenおよび約5,800 tokenの呼び出しでも継続した。長いcontextが
   状態劣化の契機だった可能性はあるが、根因を断定できる証跡は残っていない。
6. 回避策として、coordinator が実推論の進捗を監視し、異常時に既存の安全な demote と
   auto-promote を組み合わせて distributed DS4 を両nodeで再生成し、canary推論による
   回復確認後に受付を再開する方式を推奨する。

## 3. 障害の概要

対象ジョブは次のとおりである。

| ジョブ | 開始時刻 (JST) | 終了時刻の目安 | 結果 |
|---|---:|---:|---|
| `gpt-realtime-news` | 02:30 | 03:27 | failed |
| `openai-news-daily` | 04:00 | 04:52 | failed |
| `ds4-scheduled-update` | 06:00 | 07:15 | failed |

三件とも最終的に次のエラーとなった。

```text
RuntimeError: Provider has been unresponsive (no response received) for 5
consecutive stale attempts — aborting this call to avoid an indefinite stall.
Switch models or start a new session, then retry.
```

## 4. 確認した因果関係

### 4.1 Hermes の実行経路

Hermes の cron ターンは direct API call を選択し、非 streaming で完全なresponseを待つ。
`deepseek-v4-flash`には reasoning model の stale 下限600秒が適用されるため、ローカル
endpointであっても、この呼び出しでは600秒のstale timeoutが有効になっていた。

各ジョブでは約600秒で接続が切断される処理が5回続き、6回目の直前に連続stale回数の
上限へ到達した。Siderostat の `proxy_request` log に残った約600,000 msのHTTP 500と、
Hermes側のstale記録は時刻と回数が対応する。

### 4.2 DS4 の速度低下

障害前後に確認できた主な推論は次のとおりである。

| 状態 | prompt / cacheの概数 | decode速度 |
|---|---:|---:|
| 障害前 | prefill 22,272 | 約27 tokens/s |
| 障害前 | prefill 14,341 + cached 22,692 | 約26.5 tokens/s |
| 障害前 | prefill 5,446 + cached 37,904 | 約26 tokens/s |
| 低速化の境界 | prefill 3,973 + cached 44,171 | 約0.17 tokens/s |
| 04:00ジョブ | prefill 24,934 + cached 4,493 | 約0.17 tokens/s |
| 06:00ジョブ | prompt 約5,824、cacheなし | 約0.16 tokens/s |

低速化の境界となった呼び出しでは、prefill完了後、最初の50 tokenに約290秒、100 tokenに
約577秒を要した。後続の小さいpromptでも同じ低速状態が続いたため、単に長いpromptの処理量が
増えただけでは説明できない。

### 4.3 Hermes 初期報告からの訂正

初期報告は `/cluster` の `peer_ingress_ready: false` を異常の根拠の一つとして扱ったが、
調査対象commitではこのfieldは実測値ではなく固定値である。この値からpeer ingressの障害を
推定してはならない。

また、現行metrics実装の `response_header_ms` と `first_body_byte_ms` は、どちらも同じ
time-to-first-byteから設定される。両者の差を用いた障害分類は現時点ではできない。

## 5. worker側で確認した事項

MacBook Proの失敗時間帯について、power、network、process、thermalの記録を確認した。

### 5.1 Sleep / Wake

- 02:30--07:15に Sleep、Wake、DarkWake の記録はなかった。
- 06:23に生成されたdiagnostic reportにも、machineがsleepしていない旨が記録されていた。
- Siderostat workerは障害時間帯を通じて同じgenerationおよびPIDで稼働していた。

したがって、workerのsleepによる分散処理停止は否定される。

### 5.2 Thunderbolt bridge

- `bridge0`のbase RTTは概ね1 msだった。
- TCP再送はほぼなく、数百MB規模の転送も約0.1--1秒で完了していた。
- 対象時間帯にlink down/up eventはなかった。

少なくとも通常のTCP/IP logで観測できる継続的なlink劣化は認められない。

### 5.3 電源とthermal

- AC電源接続、Low Power Mode無効だった。
- 02:48:39--02:49:47に短いthermal level 1の記録があった。
- thermal eventは低速化開始後で、持続的なperformance warningではなかった。

この短いthermal eventだけでは、その後数時間続いた約150分の1のdecode速度を説明できない。

## 6. macOSセキュリティアップデートとの関係

`/var/log/install.log`から確認できた時系列は次のとおりである。

| 時刻 (JST) | 事象 |
|---:|---|
| 01:48 | 自動更新の確認。26.6.2を検出したがdownload対象なし |
| 07:44 | 再度確認。download対象なし |
| 08:34:41 | ユーザー操作によるinstall開始 |
| 08:34:46 | 約2.9 GBのdownload開始 |
| 08:41:16 | download完了 |
| 08:42:21 | logout後のinstall開始 |
| 08:43 | shutdown |
| 08:46 | reboot |
| 08:47以降 | macOS 26.6.2 build 25G83で稼働 |

最後のcron失敗は07:15ごろであり、実際のdownload開始より前である。01:48と07:44の処理は
短いscanのみで、background downloadは開始されていない。したがって、更新処理によるCPU、
disk、network負荷を障害原因とはみなさない。

一方、障害後の正常化はworkerの再起動とOS更新後に確認された。この一回の観測だけでは、
次の二つを区別できない。

- 再起動でDS4、Metal、KV cache等の一時的状態が初期化された。
- macOS 26.6.2の変更がMetalまたはGPU処理へ影響した。

26.6.2で複数夜にわたり再発しない場合も、OS更新による改善を確定したとは扱わず、同じ
workloadと観測条件で評価する必要がある。

## 7. 根因の評価

### 7.1 確定できる範囲

```text
DS4 decodeの極端な低速化
  -> 非streaming responseが600秒以内に完成しない
  -> Hermesが接続を切断
  -> 同じ状態で5回再試行
  -> circuit breakerがRuntimeErrorを返す
  -> cron job failed
```

### 7.2 現時点の有力仮説

約48,000 tokenのcontextを処理した時点で、distributed decode、KV cache、Metal runtime、
またはDS4内部sessionのいずれかが低速状態へ入り、その状態が後続requestにも残留した可能性が
ある。小さい新規requestでも低速だったため、Hermesの特定promptだけに閉じた問題ではない。

ただし、障害中のGPU/Metal counter、memory pressure、DS4 stack sample、両nodeのsession状態を
十分な粒度で保存できていない。どのcomponentが状態を保持したかは確定できない。

## 8. 当面の運用回避策

### 8.1 cron前canary

各cronの5--10分前に、同じSiderostat endpointへ32--64 token程度を生成する短いrequestを
送る。`/healthz`、`/readyz`、`cluster doctor`はprocessやrouteの健全性を確認するものであり、
推論速度の健全性までは保証しないため、実推論が必要である。

初期閾値は次を候補とする。

- 30秒以内に規定token数を完了できない。
- active generationでdecodeが5 tokens/s未満となる。
- token進捗が60秒以上更新されない。
- HTTP 5xxまたはtimeoutとなる。

通常時約26 tokens/s、障害時約0.17 tokens/sに対し、5 tokens/sは大きなmarginを持つ。
実測を蓄積した後、model、context、prefill量ごとの基準へ調整する。

### 8.2 coordinator主導の再構築

現行SiderostatにはcoordinatorからworkerのSiderostat processだけをrestartするcontrol commandは
ない。また、distributed stateでlocal adminの `cluster restart` を呼ぶと、active lifecycle
ownerを迂回しないよう意図的に拒否される。

一方、coordinatorは既存のdemotion lifecycleにより、次を実行できる。

1. 新規requestのadmissionを停止する。
2. local requestとworkerをdrainする。
3. coordinator側distributed childを停止する。
4. 認証済みcontrol channelでworkerへ`Demote`を送り、worker側distributed childを停止する。
5. 両nodeをstandalone readyへ戻す。
6. `auto_promote = true`によりpairとpromotionを再実行する。
7. 新しいgenerationの`DistributedReady`へ収束させる。

distributed pipelineは両nodeのchildで一組である。worker childだけを再起動してcoordinator childを
残す方式より、既存のdemoteとauto-promoteで両方のdistributed childを再生成する方が、connection、
generation、KV/session状態の不整合を避けやすい。

運用上は、canary失敗時にcoordinatorでdemoteを一度だけ実行し、`DistributedReady`への再収束と
二回目のcanary成功を待ってから本来のcronを開始する構成を先行導入できる。

### 8.3 Hermes側のretry方針

同じ低速backendへ600秒単位で5回retryするのではなく、次の順序へ変更する。

```text
最初のstaleまたは事前canary失敗
  -> coordinatorの復旧操作を一回実行
  -> DistributedReadyとcanary成功を確認
  -> 本requestを一回retry
  -> 回復しなければ代替modelまたは次回実行へ退避
```

`HERMES_API_CALL_STALE_TIMEOUT`を1800秒などへ延長するだけの対策は推奨しない。0.17 tokens/sで
数千tokenを生成すると数時間を要し、失敗判定が遅れるだけになる可能性が高い。

## 9. Siderostat改善提案

### 9.1 `recover-degraded`操作

汎用的なremote process restartではなく、coordinatorだけが実行できる
`recover-degraded` admin操作を追加する。

想定する状態遷移は次のとおりである。

```text
DistributedReady
  -> DegradedObserved
  -> RecoveryDraining
  -> PairedStandaloneReady
  -> Promoting
  -> DistributedReady
  -> CanaryHealthy
```

実装は既存のdemotion ownerを再利用し、control protocolを迂回してworker processへsignalを
送らない。必要なら復旧理由を`throughput-degraded`としてmetricsとstructured logへ記録する。

### 9.2 検知器

既存のcoordinator metricsには次がある。

- `ds4_proxy_ds4_generation_active`
- `ds4_proxy_ds4_generation_chunk_tps`
- `ds4_proxy_ds4_generation_avg_tps`
- `ds4_proxy_ds4_generation_elapsed_seconds`
- request durationおよびtime-to-first-byte

ただし、decode progress logがまだ一度も出ていないfirst-token stallでは、TPSだけによる検知が
遅れる。検知器には次の二経路が必要である。

1. cron前の明示的なcanary timeout。
2. active requestに対する「最後のprefill/decode進捗からの経過時間」。

後者のため、現在値だけでなく、最後にprogress eventを観測したmonotonic timestampとtoken countを
保持する。idle時の0 tokens/sを異常判定してはならない。

### 9.3 monitorのメニューバー表示

monitorのメニューバーでprefillまたはdecodeの進行中に表示する代表TPSは、現在の平均値
（prefillの`avg_tps`およびdecodeの`avg_tps`）から、直近chunkの値
（prefillの`chunk_tps`およびdecodeの`chunk_tps`）へ変更することを提案する。

累積平均はrequest開始からの正常な区間を含むため、途中で急激に低速化した場合に表示値への
反映が遅れる。直近chunkのTPSであれば現在の処理速度をより直接的に示し、本障害のような
throughput degradationをメニューバーから早期に認識しやすい。具体的には既定の
`live_metric`を`prefill-chunk-tps`とし、prefillが進行中でない場合のdecode fallbackにも
`generation_chunk_tps`を採用する。

chunk値は平均値より変動しやすいため、`avg_tps` metricとメニュー内の詳細表示は診断用として
残す。また、表示値の更新時刻またはlast progress ageを併記できるようにし、最後に観測した
chunk値が進捗停止後も現在値に見えることを避ける。自動復旧の判定はメニューバー表示値そのもの
ではなく、9.2の継続時間、複数sample、last progress timestampを用いる。

### 9.4 安全弁

自動復旧には少なくとも次の制約を設ける。

- `DistributedReady`でのみ自動復旧を開始する。
- 同時に一つだけのrecovery ownerを許可する。
- 低TPSを一回観測しただけでは実行せず、継続時間または複数sampleを要求する。
- recovery cooldownを設ける。初期値は1時間を候補とする。
- 一晩または一定時間内の自動復旧回数に上限を設ける。
- 連続失敗時はrestart loopへ入らず、健全なstandaloneを維持するかmanual interventionへ移る。
- 復旧開始前にdiagnostic snapshotを保存する。
- active requestのdrain timeout時に何を中断できるかを明文化し、無条件のprocess killを行わない。

### 9.5 診断snapshot

状態を初期化する前に、少なくとも次を同一recovery IDで保存する。

- coordinator/workerのcluster generation、control generation、phase、lease
- distributed childのPID、generation、起動時刻
- 直近のprefill/decode progress、TPS、token count、経過時間
- requestのin-flight数、TTFB、status、cancel/timeout理由
- process RSS、memory pressure、thermal/power状態
- `bridge0`のlink状態、RTT、再送および転送量
- DS4およびSiderostatのbinary digest、macOS build

prompt、response body、API key、token、session ID等は保存しない。

## 10. 段階的な導入案

### Phase 0: 運用スクリプト

- cron前canaryをcoordinatorで実行する。
- timeout時は証跡を保存し、既存のdemoteを一度だけ実行する。
- `DistributedReady`を期限付きで待つ。
- canary再成功時だけ本ジョブを開始する。
- 失敗時は通知し、代替modelへ切り替える。

Siderostat本体のstate machineを変更せず、閾値の妥当性と復旧成功率を確認できる。

### Phase 1: 観測強化

- last progress timestampとtoken deltaをmetricsへ追加する。
- monitorのprefill/decode代表TPSを直近chunk値へ変更し、平均値は詳細表示に残す。
- throughput degraded、recovery started/completed/failedをstructured event化する。
- recovery前snapshotを自動保存する。

### Phase 2: coordinator内蔵自動復旧

- `recover-degraded` admin actionと単一ownerを実装する。
- demote、再promotion、canary、admission再開を一つのjobとして追跡する。
- cooldown、回数上限、manual fallbackを設定化する。

### Phase 3: Hermes連携

- Hermesが最初のstaleで同じrequestを反復する前に、Siderostatのrecovery jobを確認する。
- 復旧成功後に一回だけretryする。
- 復旧失敗時は別modelへfallbackする。

## 11. 検証項目

実装時には少なくとも次を検証する。

1. 通常の短いrequestやprefill中に誤って復旧しない。
2. 低TPS、first-token stall、progress停止をそれぞれ検知できる。
3. active requestがないcanary失敗から、期限内に新しいgenerationの`DistributedReady`へ戻る。
4. active requestがある場合、admission、drain、timeoutの順序が保証される。
5. demoteまたはpromotion失敗時にrestart loopへ入らない。
6. workerまたはcontrol channelが不通でも、coordinatorが安全なstandaloneへ収束する。
7. recovery後のcanaryが閾値を満たさなければ正常扱いしない。
8. recovery前後のdiagnostic snapshotからPID、generation、TPSの変化を追跡できる。
9. worker-only process restart、coordinator-only process restart、両node restartの既存再接続試験を
   維持する。
10. monitorがprefill/decodeの進行中に直近chunk TPSを代表値として表示し、進捗停止後の古い値を
    現在値として表示し続けない。

## 12. 次回再発時の切り分け

次回はmacOSを再起動する前に、次の順序で停止点を設ける。

1. diagnostic snapshotを取得する。
2. 同じendpointへ短いcanaryを送って低速を再確認する。
3. worker側distributed DS4 childだけを安全に停止・再生成できる検証環境では、その結果を測る。
4. 改善しなければ既存demote/auto-promoteで両側distributed childを再生成する。
5. それでも改善しなければSiderostat process、最後にmacOSの順で再起動する。

各段階で同じcanaryを実行することで、状態を保持していた範囲をworker child、distributed pair、
Siderostat、OS/Metalのいずれかへ絞り込める。

## 13. 関連資料と実装箇所

- [`../operations.md`](../operations.md): status、metrics、safe restartの運用手順
- [`../compatibility/reconnect-acceptance-2026-08-17.md`](../compatibility/reconnect-acceptance-2026-08-17.md): 実機再接続試験
- [`../spec.md`](../spec.md): cluster lifecycle、metrics、security contract
- `src/metrics.rs`: prefill/decode progress metrics
- `src/cluster/control.rs`: 認証済みcontrol command
- `src/cluster/production/pairing.rs`: coordinatorのpromote/demote入口
- `src/cluster/production/effects.rs`: worker側の`Demote` effect
- `src/app.rs`: admin actionおよびdistributed時のrestart制約

## 14. 判定

本障害のfailure chainは説明できるが、DS4/Metal/KV/sessionのどこで低速状態が発生したかは
確定していない。現時点では原因断定を待って長時間retryするより、実推論による早期検知、
既存lifecycleを使ったcoordinator主導のdistributed再構築、回復確認、回数制限付きfallbackを
導入することが妥当である。
