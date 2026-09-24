# H06 Manager Executor Bridge Design

## 目的

H06 の残件は、monitor の Manager GUI が job を送信できても、runtime の
`POST /manager/jobs` が `JobJournal` に登録するだけで実処理を開始せず、
`running` から `succeeded` / `failed` へ遷移しないことである。本設計では、
runtime 内に明示的な manager executor bridge を追加し、既存の manager domain
関数を呼び出した結果を `JobJournal` へ反映する。

成功条件は次のとおりとする。

1. submit 直後に job が `running` として観測できる。
2. executor が処理を引き受けた job は、成功または理由付き失敗へ必ず終端する。
3. cancel は実行中処理へ伝播し、キャンセルされた job は成功扱いにならない。
4. 必要な実機入力・artifact・generation・lease が無い場合は `failed` へ遷移し、
   GUI が誤って成功表示しない。
5. executor のHTTP処理とAppKit UI処理は分離され、runtimeの既存childをmanagerが
   直接 signal しない。

## 範囲と制約

- 既存 `JobJournal` の公開DTOと `/manager/*` の認証・HTTP形状を維持する。
- `fetch/build/download/verify/stage/activate/rollback` の各 job を同じ executor
  境界から扱う。
- 既存の `src/manager/source.rs`、`build.rs`、`download.rs`、`verify.rs`、
  `stage.rs`、`activation.rs`、`rollback.rs` の検証規則を再実装しない。
- manager はchildを直接停止・起動せず、activation/rollback は既存の
  `ProductionClusterRuntime` 境界へ委譲する。
- 未設定の本番入力をfixture成功へ置き換えない。入力不足や未対応の実機操作は
  typed error として終端する。
- 既存runtime、model、登録済みstateを自動削除・上書きしない。

## アーキテクチャ

### コンポーネント

`src/manager/executor.rs` に次の境界を追加する。

- `ManagerExecutionRequest`: job id、kind、payload key、activation用の
  generation/lease、キャンセル共有フラグを保持する。
- `ManagerExecutionBackend`: 1 job のdomain処理を実行するtrait。実機用backendと
  test fixture backendを差し替え可能にする。
- `ManagerExecutor`: bounded channelからrequestを受け、backendをworker taskで
  実行し、進捗・成功・失敗をjournalへ反映するhandle。
- `ManagerExecutorError`: queue停止、入力不足、domain失敗、cancelを秘密値を
  含まない表示用errorへ変換する型。

`AppState` は `JobJournal` と同じ共有所有権で `ManagerExecutorHandle` を保持する。
submit handler は次の順序で処理する。

1. bearer認証とJSON厳密parseを行う。
2. `manager::api::submit` で重複・activation busy・generation/leaseを検証する。
3. journalへjobを作成した後、同じjob idをexecutorへ送る。
4. queueが停止している場合は、そのjobを即座に `failed` とし、202形状を壊さずに
   状態照会で理由を返す。

cancel handler はjournalを `cancelling` にした後、executorのcancel registryへ
   同じjob idを通知する。executorはdomain関数へ共有 `AtomicBool` を渡し、
   `Canceled` / `cancel=true` を成功より優先する。

### 入力解決

現在のAPIの `payload_key` は同一作業のidempotency keyであり、秘密や任意shell
文字列を受け付けない。この契約を維持するため、executorはkindごとの入力解決を
`ManagerJobInputResolver`へ分離する。

- resolverは許可済みのpayload keyを、設定・managed namespace・固定catalog・現在の
  runtime snapshotから具体的なdomain requestへ変換する。
- keyが未知、必須artifactが無い、catalog checksumが無い、またはgeneration/lease
  が不整合の場合は `MissingInput` / `RejectedInput` を返す。
- 変換後のrequestだけを既存domain関数へ渡し、URL・path・make target・roleは
  既存allowlist検証を通過させる。

この段階では、実modelが未配置のprofileやexample catalogしか存在しない環境を
成功扱いにしない。source/buildなど入力が揃う操作はdomain処理を実行し、download
やprofile操作は実artifactが無い場合にpending相当の失敗理由を返す。fixture backend
は同じrequest/result型を使い、実ネットワークや実childを起動せずに終端遷移を検証する。

### 状態遷移

```text
submit -> running
           |-- backend success ----------------> succeeded (progress=100)
           |-- domain/input error --------------> failed
           |-- cancel requested ----------------> cancelling -> failed(canceled)
           |-- executor queue unavailable ------> failed(executor unavailable)
```

terminal状態になったjobの重複keyは既存 `JobJournal` の規則で解放する。executorは
terminal更新を1回だけ行い、cancel後のlate successを許可しない。

### activation / rollback

activationとrollbackは通常jobと同じqueueを使うが、resolverが
`expected_generation` と `runtime_lease` を必須検証する。実行本体は
`ProductionClusterRuntime` の既存activation/recovery境界へ渡し、manager executor
自身はchild signalやactive artifactの直接変更を行わない。境界が接続されていない
場合は `executor backend unavailable` として失敗し、GUIは準備中・失敗理由を表示する。

## エラー処理と安全性

- journalへの更新は各jobについてsuccess/failureのどちらか一度だけ行う。
- backendのエラーはURL認証情報、bearer、raw build log、環境変数を含まない
  redacted messageへ変換する。
- queue full、worker panic、backend切断は全て該当jobをfailedへ閉じる。別jobの
  状態は変更しない。
- cancelはドメイン処理の共有flagを立てるだけで、future dropや強制killをcancel
  の代替にしない。child操作が必要なbuildは既存 `GroupRunner` の境界を使う。
- executorの状態はruntime process内のjob journalを正本とし、既存の永続cluster
  policy/state schemaを変更しない。

## テスト計画

1. executor単体: success、domain failure、cancel、queue failure、late success防止。
2. API統合: submit後にstatusがrunningからterminalへ遷移し、cancel後にsucceededへ
   進まないこと、activation/rollbackのcontext欠落拒否。
3. fixture backend: fetch/build/download/verify/stage/activate/rollbackの各kindを
   fixture入力で一度ずつ終端させる。
4. monitor契約: terminal statusをGUIの既存view modelが保持し、失敗理由をredactする。
5. 既存root/monitorの全テスト、format、clippy、diff check。
6. 実機ではまず非破壊のfetch/build/verifyを確認し、実model不足はpendingとして
   証跡へ記録する。activate/rollbackはgeneration・lease・旧artifactの事前条件が
   揃う場合だけ別途受入する。

## H06判定

executor bridgeの実装とfixture/APIテストが完了しても、実機のmodel catalog、
source checkout、activation runtime bridgeが未接続ならH06全体をPASSへ変更しない。
GUIから実処理を投入し、jobがterminalになり、clean root・失敗build・起動失敗・
upgrade失敗復元の各caseを実機で確認できた時点で、VaultのH06証跡を更新する。
