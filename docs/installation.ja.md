# Siderostat 導入ガイド

英語版（正本）: [docs/installation.md](installation.md)

## 必要なもの

- 対応する macOS を搭載した Apple シリコン Mac 2台
- Thunderbolt ケーブルと、両方の Mac で有効にした Thunderbolt ネットワーク
- 承認済みの取得元から用意した、対応する推論サービスとモデル
- 両方の Mac で同じ `Siderostat-0.3.3-no-timestamp.pkg`

初回ビルドと準備確認が完了するまで、Mac がスリープしないようにしてください。

## ビルドとインストール

確認済み source リビジョンから package を一度作成します。

```sh
cargo xtask app-dev --version 0.3.3 --build-number <単調増加するbuild番号> --verify
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.3 \
  --build-number <単調増加するbuild番号> \
  --application-identity "Developer ID Application: <name> (<team>)" \
  --installer-identity "Developer ID Installer: <name> (<team>)" \
  --timestamp-mode none \
  --output-dir dist/hotfix-0.3.3
```

`dist/hotfix-0.3.3/Siderostat-0.3.3-no-timestamp.pkg` を変更せず両方の Mac へコピーします。
各 Mac で package をダブルクリックして macOS Installer を起動し、管理者認証を完了します。
インストール後は Installer がアプリケーションを起動します。アプリケーション自身が bundle 内の
runtime helper とメニューバーの Login Item を Service Management へ登録します。

`--timestamp-mode none` は Developer ID 署名済みですが、Apple secure timestamp、公証、staple はありません。
0.x の管理された hotfix artifact であり、公開 Gatekeeper 対応配布物ではありません。旧来の source 導入が残っている場合は、
package を開く前にその checkout から `cargo xtask uninstall` を一度実行します。保持される設定、secret、モデル、実行状態、cache は削除されません。

両方の Mac が通常の単独稼働状態になってから Thunderbolt ケーブルを接続してください。

## 更新

確認済みリビジョンから新しい package を作成し、両方の Mac で macOS Installer を使って開きます。
package の導入に `cargo xtask install` は使用しません。設定、認証情報、モデルファイル、実行状態、cache は保持されます。
更新のためにこれらを削除しないでください。新しい package と既存の設定またはモデルに互換性がない場合、Siderostat は安全のため状態を進めず、単独稼働を維持します。

## ロールバック

以前に確認した package を両方の Mac で macOS Installer から開きます。Mac 2台を再接続する前に、単独稼働の
準備完了を確認してください。異なる source リビジョンから作成した package を分散ペアで混在させないでください。

## アンインストール

旧来の source 導入を削除する場合は、Siderostat の source checkout で次を一度実行します。

```sh
cargo xtask uninstall
```

source 導入で登録した旧ユーザーサービスを停止・無効化します。設定、認証情報、モデルファイル、実行状態、cache は保持します。
エラーが表示された場合は、表示された状態を解消して再実行してください。保持されたデータを削除したり、無関係なプロセスを停止したりしないでください。

## インストール結果の確認

Installer 完了後もメニューバーのモニターが表示されます。通常は次の順に進みます。

1. 相手の Mac が利用できない間は `Solo Standalone`
2. 2台の認証が完了すると `Paired Standalone`
3. 分散稼働の準備が整うと `Distributed (layer-parallel)`

安全に分散稼働へ移行できない場合は、各 Mac が単独稼働を続けます。これは安全機能による通常の動作であり、
サービスの別コピーを起動する必要はありません。
