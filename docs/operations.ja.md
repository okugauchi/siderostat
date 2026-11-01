# siDeroStat 運用ガイド

この文書は、siDeroStat の日常的な利用方法を説明します。

英語版（正本）: [docs/operations.md](operations.md)

## メニューバーのモニターが表示する内容

メニューバーのモニターには、現在の動作状態、準備状態、接続状態、現在の推論の進行状況が表示されます。
処理速度は進行中の処理に対する値であり、処理終了後に現在値として残り続けるものではありません。

長時間進行が届かない場合、モニターに stale または stalled の警告が表示されることがあります。
その場合は新しい処理を開始せず、以下の復旧手順を確認してください。

動作状態は、2台の Mac がどのように動作しているかを表します。

| 状態 | 意味 |
|---|---|
| `Solo Standalone` | この Mac だけで推論を提供しています。 |
| `Paired Standalone` | 2台は接続・認証済みですが、分散稼働の準備中です。 |
| `Distributed (layer-parallel)` | 2台が協力して1つの推論を処理しています。 |
| `Distributed (tensor-parallel)` | 両 TP child の RDMA 接続と生成 warm-up が完了しています。 |

モデルの詳細情報は動作状態とは別です。`MXFP4` はモデルの量子化、`DSpark` は投機実行のサポートを表します。
どちらもトポロジー名や動作状態名ではありません。

## 通常のメニュー操作

メニューには次の操作があります。

- `siderostat-runtimeを再起動`: 設定を保持したまま、管理対象の推論サービスを再起動します。再起動中は短時間、
  単独稼働または offline と表示されることがあります。
- `siderostat-runtimeを起動して自動起動を有効化`: サービスを起動し、バックグラウンドでの自動起動を有効にします。
- `siderostat-runtimeを停止して自動起動を無効化`: サービスを停止し、バックグラウンドでの自動起動を無効にします。
  この操作はログイン項目のリストから siDeroStat を削除しません。
- `ログイン項目を開く`: システム設定 > 一般 > ログイン項目を開きます。いつでも実行できます。
- `siDeroStatを終了`: メニューバーのアプリケーションだけを終了します。ユーザーデータを削除する操作ではありません。

siDeroStat のメニューバーアプリを2つ起動しないでください。推論サービス用の別の起動項目も作成しないでください。

## ローカル API の利用

同じ Mac 上のアプリケーションから、次の OpenAI 互換エンドポイントを利用できます。

```text
http://127.0.0.1:18080/v1
```

このエンドポイントは既定では Mac 自身からだけ利用できます。起動中や動作状態の切り替え中は、要求が HTTP 503 または
HTTP 504 になることがあります。失敗した要求は siDeroStat が再実行しないため、再試行が安全かは利用するアプリケーション側で判断してください。

## ケーブル接続と分散稼働

Thunderbolt 接続後、siDeroStat は接続、認証、モデル互換性、サービス準備状態を順に確認します。ケーブル接続の信号だけで
分散稼働が安全だと判断することはありません。

ケーブルを外した場合や相手の Mac が利用できなくなった場合は、単独稼働へ戻ります。再接続後は、長時間のジョブを開始する前に
メニューバーの状態が落ち着くまで待ってください。

## 接続モード

接続モードは動作状態とは独立し、メニューバーから選択します。

- `Automatic`（自動）— 接続状態に従います。既定値です。peer の準備が整うと分散稼働へ移行し、
  利用できなくなると単独稼働へ戻ります。
- `ForcedStandalone`（強制単独）— 相手が見えていてもこの Mac を単独に保ちます。このモード中は
  TP・pairing・promotion を開始せず、選択は再起動後も維持されます。

選択したモードは operation-policy API 経由でクラスタに適用されます（表示専用ではありません）。
ポリシー変更の進行中は、メニューに保留中のジョブと busy 理由を表示します。長時間ジョブの前に
`ForcedStandalone` を選ぶと、peer の状態変化からこの Mac を隔離できます。

## DS4 マネージャー

DS4 の source・model・activation を管理する画面は v0.4.0 に含まれません。
v0.5.0 以降に再検討します。

## Web Search Bridge

Responses API 用の任意の Web Search Bridge（SearXNG 基盤）を提供します。既定でオフ、
外部アクセスは opt-in、SearXNG は自動インストール・起動しません。検索には上限があり、
推論サービスの通常の要求待ち行列を迂回しません。Bridge の状態と SearXNG の health は
モニターに表示されます。

## 復旧と canary 確認

復旧が必要な場合、siDeroStat はサービスが応答できるかを確認します。通知や診断出力にプロンプト、応答、認証情報、
API キーを含めません。

自動的な縮退復旧は既定で無効です。管理者が有効にした場合も、試行回数と次の試行までの cooldown に上限があります。復旧に失敗した場合は、
Mac を繰り返し再起動せず、要求受付を閉じたままにします。手動復旧を求める通知が表示された場合は、新しいバッチ処理を開始せず、
実行中の要求が終わるのを待って管理者へ連絡してください。

重要な長時間ジョブを開始する前に、両方のメニューバーモニターが最終的な準備完了状態を表示していることを確認してください。
復旧通知が残っている場合はジョブを開始しないでください。

## 安全とプライバシー

- siDeroStat や推論サービスの起動項目を追加で作成しないでください。
- サービス稼働中に siDeroStat のアプリケーションサポートデータを削除しないでください。
- ファイルを手動で削除したり、無関係なプロセスを停止したりせず、`cargo xtask uninstall` を使用してください。
- 推論本文、認証情報、API キーは siDeroStat の通知に含まれません。

## Tensor-parallel の運用

TP は `ds4.distributed.topology = "tensor-parallel"` と `transport = "rdma"` で明示的に選びます。
既定は LP です。両ノードにこの起動経路の実装が必要です。既存の control/discovery address は
認証済み制御接続用です。RDMA 接続には別に `rdma_member_interface`、`rdma_address`、
`rdma_device`、`rdma_gid`（数値の GID index）を指定します。bridge のアドレスだけでは
起動できません。siDeroStat は設定を読み取り確認し、RDMA の有効化やネットワーク変更を行いません。

`model_manifest` は schema 3 の TP deployment を指します。`capability_manifest` には
同じ source revision、model family、transport、role artifact の明示的で stable な TP 対応が
必要です。worker は承認済み `ds4`、coordinator は `ds4-server` を起動します。
任意の `role_artifact` は共有 deployment と承認済み identity が一致する場合だけ、ローカルの
実行ファイルのパスを置き換えます。モデルの hash/size、context、binary hash、distributed help の
digest を起動時の cleanup より前に検証します。これらのメタデータは別途作成・検証が必要です。
今回の変更は特定モデルの設定や動作保証を含みません。

LP の layer range、DSpark/SSD 引数、TCP、管理対象 TP 引数の上書きは指定できません。
context、device、GID index は実際の child の起動引数に反映されます。
縮退先の Standalone は、その実行ファイルと設定だけで動作できる状態にしてください。

起動時は新規受付を閉じて処理中の要求を待ち、worker 準備、coordinator 起動、両 child と RDMA
接続の確認、HTTP 準備、内部の期限付き1-token生成を順に実行します。全段階の成功後に公開します。
起動失敗時は受付を閉じ、所有 child を停止して Standalone の動作を確認してから受付を再開します。
稼働中に child・制御接続を失った場合は受付を閉じ、`auto_demote` が有効なら同じ縮退処理を行い、
無効なら手動対応を要求します。drain、child 停止、Standalone 起動に
失敗した場合は受付を閉じたまま手動対応を要求します。自動 TP 起動は `auto_promote` に従います。
生成速度低下に対する自動復旧は別設定で、既定では無効です。

TP の失敗は初回に加えて2回まで再試行します。失敗回数と session 番号は Standalone 復帰や
自動プロセス再起動を越えて保持します。接続成功後に障害が発生した場合も回数を消費します。
明示的な runtime 再起動、ForcedStandalone から Automatic への適用済み変更、検証済み
 deployment/RDMA identity の変更で上限を解除します。reconcile 操作は安全な Standalone 復帰を
検証しますが、失敗回数を解除しません。runtime 停止時は受付を閉じたままにします。

ソフトウェア受入は2-node制御HTTPと注入した lifecycle によるものです。通常のsiDeroStat起動、
切断・再接続、Force、再起動の実機 RDMA 試験は導入前に別途必要です。モデル選定と実機受入は
今回の作業に含みません。
