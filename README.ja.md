# siDeroStat

英語版（正本）: [README.md](README.md)

siDeroStat は、2台の Apple シリコン搭載 Mac を Thunderbolt で接続し、2ノードの推論環境として
利用するためのソフトウェアです。接続の準備状態に応じて、単独稼働と分散稼働を切り替えます。
DS4 推論サービス（DwarfStar）を基盤とし、推論サービス・モデル・認証情報はローカルに保持します。

> [!NOTE]
> 現在動作確認済みのモデルは DeepSeek V4 Flash です。リリースで明示されていない他のモデルには対応していません。

> [!NOTE]
> 対応する構成は、Thunderbolt ネットワークで接続した2台の Mac だけです。3台以上の構成には対応していません。

## 主な機能

- 相手の Mac が利用できない場合は、各 Mac を単独で稼働させる
- 2台の接続状態と認証状態を確認する
- 両方の Mac の準備が整うと分散稼働へ移行する
- 接続または相手の Mac に問題がある場合は単独稼働へ戻る
- メニューバーから接続モードを選択する: `Automatic`（自動）は接続状態に従い、
  `ForcedStandalone`（強制単独）は相手が見えていてもこの Mac を単独に保つ
- Thunderbolt 接続上で Mac 間 tensor parallelism（TP）を実行する（両方の Mac とモデルが対応する場合）。
  TP は接続モード・peer プロトコル交渉・upstream capability で gate され、非対応 peer では起動しない
- マネージャー画面から DS4 の source 取得、build、model download、activation ライフサイクルを管理する。
  fetch/build/download は現在の active artifact を変更せず、起動失敗時は直前の active artifact へ
  rollback できる
- 任意の Web Search Bridge（SearXNG 基盤）を提供する。検索は既定でオフ、外部アクセスは
  opt-in、推論サービスの通常の要求待ち行列を迂回しない
- メニューバーから管理対象の推論サービスを起動、停止、再起動する
- メニューバーのモニターと通知で、動作状態、準備状態、推論の進行状況を表示する
- 推論本文や認証情報を通知・診断出力へ記録しない

## 対応する動作状態

| 状態 | 意味 |
|---|---|
| `Solo Standalone` | この Mac だけで推論を提供しています。 |
| `Paired Standalone` | 2台の接続と認証は完了していますが、分散稼働の準備中です。 |
| `Distributed (layer-parallel)` | 2台が協力して1つの推論を処理しています。 |

接続モード（`Automatic` / `ForcedStandalone`）はこれらの動作状態とは独立です。
`ForcedStandalone` は相手が見えていても単独稼働を維持し、`Automatic` は接続状態に従います。

`MXFP4` はモデルの量子化情報です。`DSpark` は投機実行のサポート情報です。これらは動作状態や
トポロジーの名称ではなく、モデルに属する詳細情報です。

## 必要なもの

- 対応する macOS を搭載した Apple シリコン Mac 2台
- 各 Mac の Rust 1.85 以降
- Thunderbolt ケーブルと、両方の Mac で有効にした Thunderbolt ネットワーク
- 承認済みの取得元から用意した、対応する推論サービスとモデル

## インストール

両方の Mac に同じ確認済みソースリビジョンを導入します。各 Mac のリポジトリ checkout で次を実行します。

```sh
cargo xtask fingerprint-models
cargo xtask install --start
```

このコマンドはローカルの runtime とメニューバーモニターをビルドし、ユーザーサービスを登録して起動します。
両方の Mac が通常の単独稼働状態になってから Thunderbolt ケーブルを接続してください。

詳細な手順は[導入ガイド](docs/installation.ja.md)を参照してください。

## siDeroStat の利用

利用するアプリケーションには、次のローカル OpenAI 互換エンドポイントを設定します。

```text
http://127.0.0.1:18080/v1
```

メニューバーのモニターで現在の状態と進行状況を確認できます。起動中や状態の切り替え中は、要求が HTTP 503 または HTTP 504 で
一時的に失敗することがあります。siDeroStat は失敗した要求を再実行しないため、安全に再試行できるかは利用するアプリケーション側で判断してください。

### 接続モード

メニューバーから接続モードを選択できます。

- `Automatic`（自動）— 接続状態に従います。peer の準備が整うと分散稼働へ移行し、
  利用できなくなると単独稼働へ戻ります。既定値です。
- `ForcedStandalone`（強制単独）— 相手が見えていてもこの Mac を単独に保ちます。
  このモード中は TP・pairing・promotion を開始せず、選択は再起動後も維持されます。

選択したモードは operation-policy API 経由でクラスタに適用されます（表示専用ではありません）。
ポリシー変更の進行中は、メニューに保留中のジョブと busy 理由を表示します。

### DS4 マネージャー

マネージャー画面から DS4 の source とモデルのライフサイクルを管理できます。

- 確認済みの DS4 source commit を取得し、runtime を build して binary identity を記録する
- モデルを resume 対応で download する。checksum を検証できないモデルは activation できない
- download したモデルを activation する。activation は単一の確認操作で、成功するまで現在の
  active artifact を変更しない。起動失敗時は直前の active artifact へ rollback できる。
  rollback 後も直前の artifact は保持される
- build 対象と失敗理由を表示する。認証情報と生の build ログはマネージャー画面に表示しない

### Web Search Bridge

Responses API 用の任意の Web Search Bridge（SearXNG 基盤）を提供します。既定でオフ、
外部アクセスは opt-in、SearXNG は自動インストール・起動しません。検索には上限があり、
推論サービスの通常の要求待ち行列を迂回しません。Bridge の状態と SearXNG の health は
モニターに表示されます。

## 制限事項

- 対応する Mac は2台だけです。
- Mac とモデルの構成は、ソースリビジョンで定められた互換性条件を満たす必要があります。
- 動作状態の切り替え中や推論サービスの起動中は、短い中断が発生することがあります。
- 自動的な縮退復旧は既定で無効です。有効にした場合も、復旧回数に上限があり、推論サービスの通常の要求待ち行列を迂回しません。
- Mac 間 tensor parallelism は実装済みで、接続モード・peer 交渉・upstream capability で
  gate されます。TP の実機検証はハードウェア/OS 承認ステップまで Pending として記録され、
  非対応 peer では起動しません。
- RDMA transport と distributed DSpark の最適化は再実装しません。TP は upstream の
  DS4 main 契約に従います。
- `serve --dry-run` は開発専用のクラスタリング確認であり、実推論を処理しません。

## エンドユーザー向け文書

- [導入ガイド](docs/installation.ja.md) · [English](docs/installation.md)
- [運用ガイド](docs/operations.ja.md) · [English](docs/operations.md)
- [トラブルシューティング](docs/troubleshooting.ja.md) · [English](docs/troubleshooting.md)
