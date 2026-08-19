# Coordinator restart中の通知反復

- 文書状態: v0.2.1 risk acceptanceと後続改善提案
- 観測日: 2026-08-18
- 判断日: 2026-08-19
- 対象実装: `259b575c2b046bc47859bc84f0b6242ea72e02a4`

## 結論

Coordinator-only process restartでは、workerの旧distributed childが停止を完了するまで約180秒
かかる場合がある。この間にcoordinatorが`SoloStandaloneReady`と`PairedStandaloneReady`へ
有限回入り直し、現在の通知mappingにより「Standalone起動」と「ノード検出」が複数回表示され得る。

この挙動は、停止不能、無限restart loop、Ready偽装、stale/orphan childではない。H-03の全cycleは
手動pair/reconcile、state削除、force killなしで新しいchildの`DistributedReady`へ収束した。
このためv0.2.1では安全性をPASSのまま維持し、通知反復を既知のUXリスクとして明示的にacceptする。
通知抑制はv0.2.1へ追加せず、後続変更として扱う。

## 発生経緯

旧候補では`ds4.allow_sigkill=false`のため、SIGTERMに応答しないworker childを180秒後にも停止できず、
recoveryが完了しなかった。v0.2.1候補では`allow_sigkill=true`とidentity確認済みowned childのreapingを
組み合わせ、停止と自動復旧を有限時間で完了できるようにした。

修正後のH-03 coordinator-only #2では、workerが旧distributed childを停止していた
13:26:29Zから13:28:59Zまで、coordinatorが約25秒周期でSolo/Pairedへ入り直した。
この区間に`pairing-ready`を7回記録し、一部のpromotion preflightはworkerの
`SoloStandaloneStarting`に対してHTTP 500となった。13:29:02ZにworkerがSolo readyとなった後は
通常のPairingとpromotionへ進み、13:30:01Zまでに両nodeが`DistributedReady`へ収束した。

## 通知が反復する理由

現在の通知serviceは、状態遷移が`SoloStandaloneReady`へ入るたびに「Standalone起動」、
`PairedStandaloneReady`へ入るたびに「ノード検出」を生成する。5秒のglobal throttleは短時間の連続投稿を
抑えるが、同じrecovery epoch内の意味的な重複を識別しない。約25秒周期の遷移はthrottle間隔を超えるため、
各通知が再び表示対象となる。

通知serviceはcluster state machineから独立した非同期の付加機能である。投稿失敗やthrottleはPairing、
promotion、admission、child lifecycleを変更しない。この分離により、通知反復はoperatorの認知負荷を
増やすが、復旧の安全性や最終収束には影響しない。

## 再現方法

実機で再現する場合は、推論requestがないchange windowで次を行う。

1. 両nodeが`DistributedReady`、admission serving、in-flight 0、lease validであることを確認する。
2. 両nodeの生成configで`cluster.timeouts.stop = "180s"`と`ds4.allow_sigkill = true`を確認する。
3. Coordinatorのruntime processだけをLaunchAgent経由でrestartする。OS、worker runtime、state file、
   model、KV cacheは変更しない。
4. 両nodeのcluster state、reconnect event、child PID/generation、通知を時刻付きで観測する。
5. 手動pair/reconcile、state削除、force killを行わず、workerがSolo readyになった後の自動promotionを待つ。

旧distributed worker childがSIGTERMへ速やかに応答した場合は反復しない。約180秒の停止経路に入った場合、
coordinatorの`pairing-ready`反復と対応する通知を再現できる。試験は最終`DistributedReady`、doctor healthy、
public API HTTP 200、新しいchild PID、stale/orphan不在まで確認する。

## v0.2.1 risk acceptance

2026-08-19にoperatorは次の条件で本リスクをacceptした。

- 観測された反復は有限で、最長の対象cycleも約4分22秒で自動収束した。
- 通知は同一recovery中に複数表示され得るが、状態機械やserving safetyを変更しない。
- H-03全4 cycle、H-02全2 cycle、H-04全3 scenarioでstale/orphan、409 loop、SIGKILL拒否はなかった。
- operatorが手動復旧を要求される状態にはならなかった。
- 通知抑制のためだけにrelease済みのrecovery lifecycleを変更するリスクをv0.2.1へ持ち込まない。

## 改善提案

後続変更では、単純な時間throttleではなくrecovery epoch単位のsemantic deduplicationを導入する。

1. peer lossまたは片側restartから最終安定状態までを一つのrecovery epochとして識別する。
2. 同じepoch内の`SoloStandaloneReady`と`PairedStandaloneReady`通知を各1回に制限する。
3. workerが`SoloStandaloneStarting`の間に生じる一時的なPairingを、operator向け「ノード検出」通知の
   確定条件にしない。
4. 最終`DistributedReady`、恒久的なSolo serving、Backoff、ManualInterventionRequired、
   DeploymentMismatchは抑制せず通知する。
5. 抑制した通知数とrecovery generationをbounded-label metricsまたは構造化logで診断可能にする。
6. 通知状態はcluster state、admission、child lifecycleの判断へfeedbackしない。

必要な回帰testは次のとおりである。

- 180秒相当のworker停止中にSolo/Pairedが複数回遷移しても、各UX通知はepoch内で1回となる。
- cable detachによる恒久的なSolo復帰は1回通知される。
- 再接続後の最終DistributedReadyは1回通知される。
- DeploymentMismatch、Backoff、ManualInterventionRequiredはdeduplicationで失われない。
- notification sender failure、GUI session不在、watch channel終了がrecoveryを阻害しない。

改善後はunit testだけでなく、coordinator-only process restartの実機再検証で通知回数、最終PID、
generation、doctor、public API、orphan不在を再確認する。
