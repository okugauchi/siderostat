# TP通常起動経路の完成

日付: 2026-10-26
状態: 承認待ち
基点: 公開済み v0.4.0

## 意図と範囲

TP設定を通常のsiDeroStat起動・自動昇格・監視・縮退処理へ接続する。
ユーザーは「モデル設定は後回しでTP起動経路だけ直す」を選択した。
特定モデルの追加、1017用設定、モデル取得、Vision、DSpark対応、性能改善、
パッケージ作成、実機への導入は今回の対象外とする。

LPの既存設定、既存manifest、公開推論APIと単独運用を維持する。
新たな独立daemonや第二の状態機械は導入せず、既存のreducer、operation lease、
policy journal、owned-child supervisorを使用する。
ただし現行OperationLeaseは種類別の並行取得を許すため、そのまま単一排他と見なさない。
既存OperationLeaseのchild切り替え操作を全種別共通のownerへ変更し、
Promotion/Demotion/Restart/Recovery/Policyと、同じchildに触れる既存Activation境界を接続する。
TPと競合する操作の同時実行を拒否する。Forceのintent永続化は待たせず、effectだけを直列化する。
LP側も同じchildを所有するため共通排他へ参加させるが、LPの起動順序・protocolは変更しない。
データ取得job等の競合しない処理まで一律に停止する変更は行わない。

## 確認した問題

- ProductionClusterRuntime::newはtopologyを問わずLP用builderを呼ぶ。
- TP用builder、RoleArtifact、TpDeploymentManifest、TP状態は存在するが通常起動から未接続。
- TP設定はlayersを禁止する一方、通常起動とmanifest生成はLPのlayersを要求する。
- promote_tpは実childを確認せずReadyイベントを順に投入する。呼び出しはテストのみ。
- TpWorkerSupervisorの接続観測がLPログに依存し、TP coordinator専用の実監視がない。
- RDMA probe、capability判定、TP recoveryも本番の開始条件・障害処理まで接続されていない。
- 過去H03は上流ds4を直接起動した試験であり、siDeroStatの通常TP起動を証明しない。

## 方針と代案

選択する方針は、既存TP部品を通常のruntimeへ接続し、LPは明示的な別経路として保持する。
ユーザーが選択した範囲に適合し、既存の排他・認証・復旧処理を共有できる。

代案の外部ds4常駐サービスはsiDeroStatによる起動・復帰を完成させないため採用しない。
LP処理をTPに偽装する方法はprotocolと準備条件が異なるため採用しない。
runtime全体の再設計は今回の目的に対して影響範囲が過大なため採用しない。

## 設定・artifactの解決

- topologyでLP/TPの読み込み・検証・起動処理を分岐する。
- LPはschema2 manifestと既存deployment identityの互換性を維持する。
- TPは既存schema3 TpDeploymentManifestを使用し、capability manifestと実binaryを照合する。
- workerはds4、HTTP coordinatorはds4-serverをrole別に解決する。単一ds4.binaryの流用はしない。
- 各roleのsource、binary digest、help契約、model identity、context、transportを確認する。
- node固有path、RDMAのinterface/device/GIDを共有deployment identityへ混入させない。
- TPのmanifest生成はLP builderを呼ばず、モデルfamilyを固定文字列で補完しない。
- contextは両roleの--ctxへ渡す。RDMAのdevice/GIDは上流契約の引数へ反映する。
- layers、HTTP worker引数、LP debug/旧DS4D引数、managed引数のextra_args上書きを拒否する。
- 設定・capabilityの不足は起動前に理由を返す。Reference/CandidateをStableとして扱わない。
- TPは明示的RDMAのみ。TCPやLPへの暗黙切り替えは行わず、既存の検証済Standaloneを復帰先とする。
- Standaloneのモデル設定と検証は既存のものを利用する。TP用モデルや1017への更新はしない。

## 起動と公開までの順序

1. 認証済peerのTP対応、deployment、policy epoch、capability、RDMA準備を確認する。
2. 既存の単一operation leaseを取得し、受付を閉じ、既存streamをdrainする。
3. 新しい単調増加TpSessionIdを割り当てる。control/cluster generationと混用しない。
4. workerの旧owned childを安全に停止し、TP workerを起動する。
5. workerの生成・生存だけをPreparedとして通知する。Connectedを待たずcoordinatorを起動する。
6. coordinator側も旧owned childを停止し、TP HTTP coordinatorを起動する。
7. 同sessionのchild identity、生存、上流source由来のTP接続完了ログ、HTTP準備を確認する。
8. 期限・回数・同時数を制限した専用permitで最小warm-upを実行する。
9. 両nodeの現session・lease・policy・child生存を再確認し、Readyを両端で確定して受付を開く。

HTTP 200のみ、ログの部分一致のみ、設定上の準備完了値のみではReadyにしない。
TP socketへgeneric TCP probeや旧DS4D HELLOを送らない。DS4のhandshake/RDMAは再実装しない。
解析するログとCLIはtests/fixtures/ds4/v040/contract.jsonの追跡sourceへ照合する。
未対応のsourceやログは推測で許可せず、互換性未確認として開始しない。

## peer制御

既存v1 LPメッセージの厳格な形式を変更しない。署名認証とreplay対策を再利用し、
TP用のversion付きnode能力確認・session付きprepare/status/ready/stopを追加する。
v2の番号だけでTP対応と判断せず、TP機能と契約版の明示一致を必要とする。
要求と応答はsessionとoperation identityで照合し、重複は冪等、旧session通知は無効とする。
workerは推論HTTPを公開せず、公開APIの要求はcoordinatorへ転送する。
旧peerとの組み合わせではTPを開始せず、Standaloneを維持して理由を表示する。

## 監視・中断・復帰

- child終了、接続喪失、lease切れ、最初のprefill/warm-up timeoutを区別する。
- 失敗を検出したら先に受付を閉じてsessionを失効させる。遅着Readyを受理しない。
- 同一incidentの復帰ownerを一つにし、既存lease下でowned childを回収する。
- PID再利用やidentity不一致を検出した場合はsignalせずManualInterventionにする。
- drain timeout時に勝手にkillへ切り替えず、既存streamの所有を維持する。
- 検証済Standaloneの準備完了後にのみ受付を再開する。復帰失敗時は503と理由を表示する。
- 通常の接続・起動障害に対する再昇格はAutomaticかつ既存auto_promoteが有効な場合だけ許可する。
  既存auto_demoteの設定を尊重し、無効時はTP障害で受付を閉じ、手動介入を示す。
  低速・stalled等を契機にするdegraded recoveryは既存opt-inのままとし、この変更で有効化しない。
- 初回試行に追加できる自動再試行は同一incidentにつき最大2回とする。
  Standaloneへの復帰成功、ケーブルの抜き差し、同一構成での自動再接続、runtimeの自動再起動では
  回数をリセットしない。note_healthy_fallbackを自動リセットとして使わない。
  incidentと残回数をjournalへ保存し、同じdeploymentとRDMA構成で復元する。
  上限の解除は明示的な手動runtime再起動、ForcedStandaloneからAutomaticへの明示変更、
  または検証済deployment/RDMA設定identityの変更に限定する。自動イベントを手動操作に偽装しない。
  手動操作がない限り、上限後はStandaloneを提供しながらTP再試行待ちの理由を保持する。
- ForcedStandaloneのintent保存後は、新規起動・継続・retryを抑止し、Standaloneへ安全に収束する。
- モード切り替え中の操作futureが破棄されてもchild停止・復帰のownerを失わない。
  起動/停止処理を短命なHTTP request futureだけに所有させない。
- restart、切断、再接続、role変更でも古いchild/session観測を再利用しない。
- dry-runは同じ制御・状態遷移をfake lifecycleで動かす。自動テストで稼働中サービスへ接触しない。

## 検証と受入

実装前に本番constructor・promotion・peer HTTP経路から現状の欠落を検出する失敗テストを作る。
Readyイベントをテストが直接投入するだけの試験を本番接続の証拠にしない。

- LP既存設定とschema2 deployment identityが維持され、既存回帰試験が通る。
- TP設定で実際に選ばれるbinary/argvがrole、context、RDMA指定と一致し、layersが存在しない。
- 本番制御経路を2nodeのephemeral HTTPと注入lifecycleで通し、worker先行を記録する。
- handshake、HTTP、warm-up、child生存、peer確認の各欠落で受付が開かない。
- capability/source/model不一致、RDMA不足、旧peerではTP spawnが0。
- HTTP readyのみ、旧session通知、旧childログ、warm-up失敗でReadyにならない。
- Force競合、child終了、peer喪失とroute喪失の同時発生でもownerは1、orphanは0。
- PromotionとRestart/Recovery/Policyのcross-kind競合が直列化される。
- 再接続10cycleでsessionが単調増加し、有限retryとManualInterventionが守られる。
- Standaloneへの復帰と自動再接続を繰り返してもTP retry budgetが復活しない。
  runtime自動再起動でbudgetが維持され、明示操作・検証済構成変更だけで解除される。
- auto_promote/auto_demote無効、ForcedStandalone、degraded recovery opt-in無効の各条件で
  禁止される自動処理が発生せず、同時に通常の既存LP動作が変わらない。
- workspace fmt/clippy/test、all-features、reconnect並列試験を変更範囲に応じて実行する。
- siderostat-reviewで指摘修正・再レビューを行う。

実機Metal/RDMA試験は別段階とし、本変更だけで実機PASS・特定モデル対応・性能を主張しない。
ソフトウェア受入と実機受入を文書・完了報告で分ける。

## 運用文書と実装者判断

公開文書のTP対応範囲、設定・manifest生成方法、未対応時の理由、停止・復帰手順を更新する。
既存資料の「直接ds4のTP実行」と「siDeroStatの通常TP起動」を区別する。
具体的な内部trait分割、ファイル分割、テストfixtureの配置は実装計画で決める。
既定LP、既存proxy timeout、recovery opt-in、release versionは変更しない。
モデル追加や実機配布が必要になった場合は別の変更として扱う。
