# DS4 Smart Proxy

DS4 Smart Proxy は、OpenAI互換のリバースプロキシを Rust で実装したものです。
ローカルに立ち上げ、複数の DwarfStar 4 (DS4) サーバーへリクエストをインテリジェントにルーティングします。

## アーキテクチャ

```
          +--------------------------+
          |      ローカルクライアント   |
          |---------------------------|
          | Codex / Hermes / curl     |
          | OpenAI SDK                |
          +------------+-------------+
                       |
                localhost:18080
                       |
                DS4 Smart Proxy
                       |
          +------------+------------+
          |                         |
      Local DS4               Remote DS4
```

- 各マシンに DS4 サーバーと DS4 Smart Proxy の両方を配置
- クライアントは常にローカルプロキシとのみ通信
- プロキシが自動的に最適なバックエンドを選択

## 機能

- **OpenAI API 互換**: すべてのエンドポイント（`/v1/chat/completions`, `/v1/responses`, `/v1/models` 等）を透過的にプロキシ
- **自動ルーティング**: ローカルバックエンドを最優先、使用中ならリモートへフォールバック
- **Heartbeat / Active Probe**: 軽量 heartbeat で到達性を確認し、active probe は障害後など推論状態が不明な場合に限定
- **ストリーミング対応**: SSE (`text/event-stream`) を検出し、バッファリングなしで透過ストリーム
- **RAII Drop ガード**: panic・タイムアウト・切断時でも確実に `in_flight` をデクリメント
- **リトライポリシー**: 接続失敗 / 5xx → 別バックエンドへ1回再試行、4xx / ストリーム開始後 → 再試行なし
- **管理エンドポイント**: `/healthz`, `/backends`, `/metrics`
- **構造化ログ**: tracing でリクエストID・選択バックエンド・レイテンシ・ステータス・リトライ回数を出力

## 要件

- Rust 1.96 以上（edition 2024）
- Apple Silicon Mac（推奨）、または任意の Linux 環境

## ビルド方法

```bash
# クローン
git clone <repository-url>
cd ds4-smart-proxy

# ビルド（リリース）
cargo build --release

# 確認
cargo check
cargo test
```

依存クレートは `Cargo.toml` に記載されており、ビルド時に自動的にダウンロードされます。

## 設定

`config.toml` を作成します。

```toml
listen = "127.0.0.1:18080"

self_name = "macbook"
tls_accept_invalid_certs = false

heartbeat_interval = "5s"
heartbeat_timeout = "2s"
heartbeat_path = "/v1/models"

active_probe_timeout = "3s"
log_timezone = "Asia/Tokyo"

[[backends]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backends]]
name = "macstudio"
url = "https://macstudio.example.internal"
max_in_flight = 1
```

| 項目 | 説明 |
|---|---|
| `listen` | プロキシがリッスンするアドレス:ポート |
| `self_name` | このインスタンスのバックエンド名（ローカル優先） |
| `tls_accept_invalid_certs` | HTTPS バックエンドの証明書検証を無効化します。自己署名証明書などローカル検証用途のみ `true` にしてください |
| `heartbeat_interval` | heartbeat 間隔（例: `5s`） |
| `heartbeat_timeout` | heartbeat タイムアウト（例: `2s`） |
| `heartbeat_path` | heartbeat に使うパス（デフォルト `/v1/models`） |
| `active_probe_timeout` | 回復確認時の active probe タイムアウト（例: `3s`） |
| `log_timezone` | ログ時刻のタイムゾーン。未指定時は `GMT`。`Asia/Tokyo`, `JST`, `UTC`, `GMT`, `+09:00` 形式に対応 |
| `[[backends]]` | バックエンド定義（複数可能） |
| `name` | バックエンド名 |
| `url` | DS4 サーバーの URL |
| `max_in_flight` | 同時実行数の上限（デフォルト 1） |

同じバイナリを全マシンで使い、設定ファイルだけを変更します。

## 使い方

### 1. 設定ファイルを作成

```bash
cat > config.toml << EOF
listen = "127.0.0.1:18080"
self_name = "macbook"
tls_accept_invalid_certs = false

heartbeat_interval = "5s"
heartbeat_timeout = "2s"
heartbeat_path = "/v1/models"

active_probe_timeout = "3s"
log_timezone = "Asia/Tokyo"

[[backends]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backends]]
name = "macstudio"
url = "https://macstudio.example.internal"
max_in_flight = 1
EOF
```

### 2. プロキシを起動

```bash
cargo run --release -- config.toml
```

またはビルド済みバイナリを直接実行:

```bash
./target/release/ds4-smart-proxy config.toml
```

### 3. クライアントからアクセス

```bash
# OpenAI SDK / Codex / Hermes から localhost:18080 を指定
# curl での確認例
curl http://localhost:18080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hello"}],"stream":false}'
```

クライアント側の設定変更は不要です。通常 DS4 サーバーを指すエンドポイントを `localhost:18080` に変更するだけです。

## 管理エンドポイント

| エンドポイント | 説明 |
|---|---|
| `GET /healthz` | ヘルスチェック（`ok` を返します） |
| `GET /backends` | 全バックエンドの状態を JSON で返します |
| `GET /metrics` | メトリクス（プレースホルダ） |

### `/backends` レスポンス例

```json
[
  {
    "name": "macbook",
    "healthy": true,
    "busy": false,
    "in_flight": 0,
    "latency_ms": 84
  },
  {
    "name": "macstudio",
    "healthy": true,
    "busy": true,
    "in_flight": 1,
    "latency_ms": 76
  }
]
```

## ルーティングポリシー

1. **ローカルバックエンド**（到達可能かつ busy でなく、cooldown / suspect 状態ではない）
2. **リモートバックエンド**（到達可能かつ busy でなく、cooldown / suspect 状態ではない）
3. 該当なし → **HTTP 503**

Hermes で観測した約600秒待機後の遅延フェイルオーバーに関する調査・実装設計は
[`docs/hermes-600s-failover.md`](docs/hermes-600s-failover.md) を参照してください。

## リトライポリシー

| 状況 | 動作 |
|---|---|
| 接続失敗 | 別の healthy なバックエンドへ再試行 |
| HTTP 5xx | 別のバックエンドへ1回再試行 |
| HTTP 4xx | 再試行なし |
| ストリーム開始後 | 再試行なし |

## ログ

`tracing` による構造化ログを出力します。環境変数 `RUST_LOG` でフィルタリングできます。

```bash
RUST_LOG=ds4_smart_proxy=info cargo run --release -- config.toml
```

各リクエストに以下の情報が含まれます:
- `request_id` (UUID)
- `backend` (選択されたバックエンド名)
- `latency_ms` (レイテンシミリ秒)
- `status` (HTTP ステータスコード)
- `retry` (リトライ回数)

## 設計原則

- インフラストラクチャ非依存（DNS / mDNS / クラウド / Kubernetes / Docker / Prometheus / Redis に依存しない）
- バックエンド検出は設定駆動型
- GPU 使用率・CPU 使用率・メモリ使用量で選択しない（DS4 の直列推論モデルに合わせる）
- 最小依存、シングルバイナリ

## ライセンス

MIT
