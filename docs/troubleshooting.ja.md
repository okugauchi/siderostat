# Siderostat トラブルシューティング

英語版（正本）: [docs/troubleshooting.md](troubleshooting.md)

## メニューバーのモニターが表示されない

確認済みの `.pkg` を macOS Installer で一度開きます。モニターの実行バイナリをターミナルから起動したり、
Siderostat の別コピーを起動したりしないでください。runtime とモニターが起動するまで待ちます。
Installer がエラーを表示した場合は、無関係なプロセスを停止していないことを確認してから同じ package を再実行します。

## モニターが offline のままになる

次の順に確認します。

1. offline と表示される Mac で `.pkg` の導入が完了していることを確認します。
2. 推論サービスの起動を待ちます。初回起動には数分かかる場合があります。
3. 相手の Mac がスリープしておらず、Thunderbolt ケーブルが両端で接続されていることを確認します。
4. システム設定で Thunderbolt ネットワークが有効になっていることを確認します。
5. すぐに再起動せず、状態が落ち着くまで待ちます。

相手の Mac が利用できない場合、`Solo Standalone` は安全な通常状態です。offline 表示を解消するために、
サービスの別コピーを起動しないでください。

## 分散稼働へ移行しない

両方の Mac が同じ確認済みソースリビジョンと互換性のあるモデル設定で稼働している必要があります。まず両方が
`Paired Standalone` になり、その後に `Distributed (layer-parallel)` へ移行します。互換性確認に失敗した場合、
Siderostat は意図的に単独稼働を維持します。

Thunderbolt ケーブルを一度外して再接続し、両方のモニターが落ち着くまで待ちます。切り替え中にモデルファイルや実行データを
変更・削除しないでください。

## 要求が HTTP 503 または HTTP 504 になる

推論サービスの起動中や動作状態の切り替え中に発生することがあります。メニューバーのモニターが準備完了状態を表示するまで待ち、
要求を繰り返して安全かどうかを利用するアプリケーション側で判断してください。Siderostat は失敗した要求を自動的に再実行しません。

## 再起動または復旧が完了しない

再起動を繰り返し選択しないでください。新しい処理を開始せず、現在の通知が完了するまで待ちます。手動復旧を求める通知が表示された場合は、
ソースリビジョン、Mac のモデル、発生したおおよその時刻、メニューバーに表示された状態を管理者へ伝えてください。プロンプト、応答、
API キー、パスワード、認証情報は含めないでください。

## ログイン後に起動しない

`.pkg` の導入が完了し、Siderostat.app が起動していることを確認します。macOS が Login Item または Background Item の承認を求めた場合は、
システム設定 > 一般 > ログイン項目を開き、Siderostat の項目を承認します。承認を変更した後は、package から導入された同じアプリケーションを再度開いてください。

## 更新またはアンインストールでエラーになる

package の更新では、確認済みの `.pkg` を macOS Installer で開き、表示されたエラーを解消してから再実行します。
package で導入したアプリケーションに `cargo xtask install` は使用しません。

アンインストールは次のコマンドです。

```sh
cargo xtask uninstall
```

package の更新と legacy source のアンインストールでは、設定、認証情報、モデルファイル、実行状態、cache は保持されます。
汎用的な削除コマンドでこれらを削除しないでください。

## 問い合わせ時に伝える情報

ソースリビジョン、Mac のモデル、Thunderbolt ケーブルの接続状態、両方のメニューバーに表示された状態、問題のおおよその時刻を伝えてください。
スクリーンショットやログを共有する場合は、プロンプト、応答、認証情報、API キー、個人パス、その他の私的なデータを削除してください。

## TP が Ready にならない

両 Mac の topology が `tensor-parallel`、transport が `rdma` であることを確認してください。
制御用 Bridge 接続と RDMA member の interface/address は別です。選択した device が active で、
数値の GID index が member の IPv4 に対応する必要があります。siDeroStat は RDMA の有効化、
アドレス割当、TP データソケットへの probe を行いません。

schema 2 の LP manifest、candidate/unknown の TP capability、不一致の source/role binary、
変更されたモデル、context や distributed help digest の不一致では TP を起動しません。
検証済みで一致する schema 3 deployment/capability/role メタデータを準備してください。
extra_args による検査の迂回はできません。worker は `ds4`、coordinator は `ds4-server` を使い、
TP 引数から LP layer range と DSpark/SSD 設定を外す必要があります。

HTTP 待受だけでは Ready になりません。両 child の生存、同じ session の RDMA 接続、内部生成
warm-up の成功が必要です。準備済み worker の起動を coordinator が中断した場合も、起動期限後に
回収・復帰します。drain、停止、縮退に失敗した場合は受付を閉じたまま手動対応を要求します。
child 診断には実際のローカル TP lifecycle が表示されます。

失敗回数は Standalone 復帰や自動再起動を越えて保持します。3回失敗した場合は原因を解消してから、
明示的な runtime 再起動、ForcedStandalone→Automatic の適用、または検証済みの新しい
 deployment/RDMA identity への変更で解除してください。reconcile は Standalone 復帰を検証しますが
回数を解除しません。受入範囲は [TP の運用](operations.ja.md#tensor-parallel-の運用)を参照してください。
