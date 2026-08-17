# Siderostat

Siderostat は、DwarfStar が提供する推論サーバー `ds4-server` が稼働する2台の Apple シリコン搭載 Mac を Thunderbolt 5 で接続することで、分散推論クラスタを簡易に構成できるソフトウェアです。OpenAI 互換 API クライアントからの要求を受け付け、Thunderbolt ケーブルの接続状態に応じて転送を変更できるプロキシサーバーとして動作します。

主なユースケースとしては、MacBook Pro と Mac Studio の連携です。例えば、外出中は MacBook Proを持ち歩き、DeepSeek V4 Flash の Q2-Q4 量子化モデルでタスクを実施します。帰宅後は、あらかじめ導入・認証・モデル配置済の Mac Studio と MacBook Pro を Thunderbolt 5 ケーブルで接続するだけで、Siderostat が2台を認識し、DeepSeek V4 Flash の MXFP4 量子化モデルによる分散推論へ切り替え、夜間バッチ処理などの計算資源として利用します。再び外出するときにケーブルを外せば、MacBook Pro は単独稼働へ戻り、Q2-Q4 量子化モデルでタスクを実施できます。Mac Studio を手動で選択したり、推論の接続先を変更する必要はありません。

> [!NOTE]
> Siderostat が `ds4-server` で動作確認しているモデルは DeepSeek V4 Flash だけです。GLM 5.2 などの他のモデルは対象外です。

> [!NOTE]
> 対応する構成は、Thunderbolt Bridge で接続した2台構成だけです。3台以上の構成や、任意の台数で構成する分散処理には対応していません。


## 主な機能

- 各 Mac で `ds4-server` を単独で起動し、推論を提供する
- 2台の接続、認証、稼働状態を自動的に確認する
- 2台が Thunderbolt 接続されたとき、分散推論へ自動的に切り替える
- Thunderbolt 接続や相手ノードに問題が起きたとき、単独稼働へ戻す
- `ds4-server` の起動、停止、再起動を管理する
- macOS の通知とメニューバーのモニターで状態や Prefill / Decode の処理速度を表示する
- 推論本文、認証情報、秘密情報をログに保存しない


## 対応する2台構成

2台の役割は、IP over Thunderbolt 接続の仮想 Bridge に割り当てた固定 IPv4 アドレスから静的に決まります。接続管理と分散処理の調整を担う協調ノード（以下、コーディネーター）と、分散処理の一部を担う作業ノード（以下、ワーカー）に分かれます。

| 役割 | 主な担当 | Thunderbolt Bridge のアドレス(例) |
|---|---|---|
| コーディネーター | 2台の接続管理、分散処理の調整 | `10.99.0.1` |
| ワーカー | 分散処理の一部を担当 | `10.99.0.2` |

役割を設定ファイルへ直接書き込む必要はありません。アドレスが未設定、重複、または想定外の場合は、安全のため2台構成の機能を開始せず、その Mac の単独稼働を維持します。

## 動作状態

### 単独稼働

相手ノードを利用せず、その Mac の `ds4-server` だけで推論します。相手ノードが停止中、Thunderbolt 接続が外れている場合も、この状態でサービスを継続できます。

### 接続済み単独稼働

相手ノードとの接続と認証は完了していますが、分散推論はまだ開始していない状態です。分散処理の準備が整うまでは、安全のため単独稼働を使用します。

### 分散推論

2台の `ds4-server` が協力して、1つの推論を処理する状態です。設定サンプルでは、DeepSeek V4 Flash の MXFP4 モデルを使用します。分散推論へ切り替えられない場合は、単独稼働へ戻ります。

状態の切り替え中は、新しい要求が一時的に HTTP 503 で拒否されることがあります。切り替えに失敗した要求を、Siderostat が別の状態で自動的に再実行することはありません。

## 導入前に必要なもの

- Apple シリコンを搭載した macOS の Mac 2台
- 2台を接続する Thunderbolt 5 ケーブル
- macOS の IP over Thunderbolt 有効化
- `ds4-server` の実行ファイル
- DeepSeek V4 Flash のモデルファイル（`download_model.sh` で Hugging Face からダウンロード可能）と対応する `ds4-server` 設定
- ビルドとインストールのための Rust 安定版実行環境

`ds4-server` とモデルの取得元、モデルの対応状況、Mac ごとの実行ファイルの確認方法は、[導入ガイド](docs/installation.md)を参照してください。

## インストール

2台それぞれで行います。モデルを配置したあと、モデルのハッシュ値を一度計算し、 `cargo xtask install` で Siderostat(プロキシサーバー本体)、macOSメニューバー常駐型のモニター、設定ファイル、macOS のログイン起動項目を配置します。

```sh
cd /path/to/siderostat
cargo xtask fingerprint-models
cargo xtask install
```

`cargo xtask install` は、80 GBを超えるGGUFファイルのハッシュ値を再計算するか確認します。既定の回答は再計算しない設定です。モデルを更新・変更した場合は、`cargo xtask fingerprint-models` を再実行してください。

2台間の `ds4-server` 確認、モデルの配置、秘密情報の共有、分散推論の確認方法は、[導入ガイド](docs/installation.md)に記述しています。インストール後のサービスの起動を同時に行う場合は、 `cargo xtask install --start` を使用します。

インストールで作成される主なファイルは次のとおりです。

- 設定: `~/Library/Application Support/siderostat/config.toml`
- 秘密情報: `~/Library/Application Support/siderostat/secrets/`
- メニューバーモニターの設定: `~/monitor.toml`
- macOS の起動項目: `~/Library/LaunchAgents/`

設定を変更して再読み込みする場合は、導入後に Siderostat を再起動してください。

## 利用と状態確認

アプリケーションからは、次の URL を OpenAI 互換 API の接続先として使用します。

```text
http://127.0.0.1:18080/v1
```

状態確認には、次のコマンドを使用できます。

```sh
siderostat cluster status
siderostat cluster doctor
curl --fail --silent http://127.0.0.1:18081/healthz
curl --fail --silent http://127.0.0.1:18081/readyz
```

`status` は現在の状態を表示し、`doctor` は推論を受け付けられる状態かを確認します。
どちらも通常は状態を変更しません。

## 通知とメニューバーモニター

メニューバーモニターは、次の情報を表示します。

- 現在の動作状態
- 入力準備の進行状況と処理速度
- KV キャッシュの利用状況
- 生成処理の処理速度
- 対象ノードが推論を受け付けられるかどうか

入力準備と生成処理の速度は、現在進行中の処理に合わせて表示されます。処理完了後に
最後の値を表示し続けることはありません。

macOS の通知では、単独稼働、2台接続、分散推論、再起動、復旧が必要な状態などを知らせます。

## 安全性と通信範囲

- 利用者向け API と管理 API は、既定ではその Mac 自身からだけ接続できます。
- `ds4-server` 自体を通常の LAN へ公開しません。
- 2台間の管理通信と分散処理通信は Thunderbolt Bridge を使用します。
- 認証用の秘密情報は、SSH 秘密鍵や PEM 形式の鍵ではありません。Siderostat 専用の認証用データです。
- 推論本文、入力内容、認証情報、完全なハッシュ値はログへ保存しません。

Thunderbolt Bridge 上の分散処理通信は暗号化されないため、専用の物理接続を信頼境界として扱ってください。

## 制限事項

- 対応する構成は2台固定です。3台以上の構成には対応していません。
- 2台の Mac はそれぞれ異なる Apple シリコン世代でも利用できますが、`ds4-server` の実行ファイルとモデル設定が、導入時に確認された互換性条件を満たしている必要があります。
- 同時に処理できる推論要求数には上限があります。既定では2件まで受け付けますが、モデル、入力の長さ、出力の長さ、Mac のメモリによって処理時間と同時実行の安定性が変わります。
- 動作状態の切り替え中や `ds4-server` の起動中は、要求が一時的に失敗することがあります。
- 要求の自動再実行は行いません。HTTP 503 や HTTP 504 を受け取った場合の再試行は、利用するアプリケーション側で判断してください。

## 関連文書

- [導入ガイド](docs/installation.md): `ds4-server`、モデル、2台構成、起動項目の導入
- [運用ガイド](docs/operations.md): 状態確認、再起動、復旧、ロールバック
- [トラブルシューティング](docs/troubleshooting.md): 接続、認証、分散推論、起動失敗の確認
- [メニューバーモニター仕様](docs/menu-bar-monitor-spec.md): 表示内容と設定
- [詳細仕様](docs/spec.md): 動作条件、通信、互換性、安全性の詳細
- [開発者向け手順](docs/development.md): ビルド、テスト、静的検査
- [設定例](siderostat.example.toml): 導入時に使用する設定のひな形
