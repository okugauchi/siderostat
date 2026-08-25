# Siderostat

英語版（正本）: [README.md](README.md)

Siderostat は、2台の Apple シリコン搭載 Mac を Thunderbolt で接続し、2ノードの推論環境として
利用するためのソフトウェアです。接続の準備状態に応じて、単独稼働と分散稼働を切り替えます。

このリポジトリではソースコードだけを公開します。公式のビルド済みバイナリ、DMG、pkg は配布しません。
利用する場合は、確認済みのソースリビジョンを各 Mac 上でビルドしてください。

> [!NOTE]
> 現在動作確認済みのモデルは DeepSeek V4 Flash です。リリースで明示されていない他のモデルには対応していません。

> [!NOTE]
> 対応する構成は、Thunderbolt ネットワークで接続した2台の Mac だけです。3台以上の構成には対応していません。

## 主な機能

- 相手の Mac が利用できない場合は、各 Mac を単独で稼働させる
- 2台の接続状態と認証状態を確認する
- 両方の Mac の準備が整うと分散稼働へ移行する
- 接続または相手の Mac に問題がある場合は単独稼働へ戻る
- メニューバーから管理対象の推論サービスを起動、停止、再起動する
- メニューバーのモニターと通知で、動作状態、準備状態、推論の進行状況を表示する
- 推論本文や認証情報を通知・診断出力へ記録しない

## 対応する動作状態

| 状態 | 意味 |
|---|---|
| `Solo Standalone` | この Mac だけで推論を提供しています。 |
| `Paired Standalone` | 2台の接続と認証は完了していますが、分散稼働の準備中です。 |
| `Distributed (layer-parallel)` | 2台が協力して1つの推論を処理しています。 |

`MXFP4` はモデルの量子化情報です。`DSpark` は投機実行のサポート情報です。これらは動作状態や
トポロジーの名称ではなく、モデルに属する詳細情報です。

## 必要なもの

- 対応する macOS を搭載した Apple シリコン Mac 2台
- 各 Mac の Rust 1.85 以降
- Thunderbolt ケーブルと、両方の Mac で有効にした Thunderbolt ネットワーク
- 承認済みの取得元から用意した、対応する推論サービスとモデル

## ソースからのインストール

両方の Mac に同じ確認済みソースリビジョンを導入します。各 Mac のリポジトリ checkout で次を実行します。

```sh
cargo xtask fingerprint-models
cargo xtask install --start
```

このコマンドはローカルの runtime とメニューバーモニターをビルドし、ユーザーサービスを登録して起動します。
Apple Developer ID の署名は必要ありません。両方の Mac が通常の単独稼働状態になってから Thunderbolt ケーブルを接続してください。

詳細な手順は[導入ガイド](docs/installation.ja.md)を参照してください。

## Siderostat の利用

利用するアプリケーションには、次のローカル OpenAI 互換エンドポイントを設定します。

```text
http://127.0.0.1:18080/v1
```

メニューバーのモニターで現在の状態と進行状況を確認できます。起動中や状態の切り替え中は、要求が HTTP 503 または HTTP 504 で
一時的に失敗することがあります。Siderostat は失敗した要求を再実行しないため、安全に再試行できるかは利用するアプリケーション側で判断してください。

## 制限事項

- 対応する Mac は2台だけです。
- Mac とモデルの構成は、ソースリビジョンで定められた互換性条件を満たす必要があります。
- 動作状態の切り替え中や推論サービスの起動中は、短い中断が発生することがあります。
- 自動的な縮退復旧は既定で無効です。有効にした場合も、復旧回数に上限があり、推論サービスの通常の要求待ち行列を迂回しません。
- Mac 間の tensor parallelism、RDMA による layer-parallel transport、distributed DSpark は v0.4+ で予定しており、v0.3.0 の機能ではありません。

## エンドユーザー向け文書

- [導入ガイド](docs/installation.ja.md) · [English](docs/installation.md)
- [運用ガイド](docs/operations.ja.md) · [English](docs/operations.md)
- [トラブルシューティング](docs/troubleshooting.ja.md) · [English](docs/troubleshooting.md)

Apple Developer ID 署名、公証、timestamp service、DMG、pkg の生成はローカル検証用の任意手順であり、
ソースリリースには含めません。
