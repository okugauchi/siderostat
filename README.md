# DS4 Smart Proxy

DS4 Smart Proxy は、複数のOpenAI互換DS4 endpointを束ねるRust製の
reverse proxy / application gatewayです。

単純なleast-busy方式ではなく、local-firstとsession/prefix affinityを組み合わせ、
長いHermesセッションのKV cache localityを保ちます。定期監視では推論を実行せず、
`GET /v1/models` だけをheartbeatとして使用します。

## 主な機能

- OpenAI互換pathと未知pathの透過転送
- local-first routing
- Hermes session、conversation、prefixによるsticky routing
- HMAC-SHA-256によるaffinity key保護
- SQLite WALによるaffinity永続化
- backendごとの推論用Semaphore
- 非推論request用の独立concurrency limit
- Unknown / Alive / Suspect / Offline / Cooldown状態管理
- bufferしないresponse streamingとSSE転送
- connect、response header、first body byte、stream idle timeout
- response開始前だけの限定的な別backend retry
- standalone DS4とdistributed coordinatorの共存
- public listenerとloopback admin listenerの分離
- 構造化ログとPrometheus互換metrics

## ビルド

Rust stable（edition 2024）が必要です。

```bash
cargo build --release
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
```

## 設定

設定はTOMLです。探索順は次のとおりです。

1. `--config`
2. `DS4_SMART_PROXY_CONFIG`
3. `./ds4-smart-proxy.toml`
4. platform既定path

完全な例は [`ds4-smart-proxy.example.toml`](ds4-smart-proxy.example.toml) を参照してください。

`affinity.database_path`の先頭では、`$HOME`、`${HOME}`、`~/`および
`$VARIABLE`形式の環境変数を展開します。参照した環境変数が未定義の場合は、
literal名のdirectoryを作成せず起動エラーになります。

affinityを有効にする場合は32 bytes以上のsecretが必要です。

```bash
export DS4_SMART_PROXY_AFFINITY_SECRET='32 bytes以上のランダム値'
cargo run --release -- --config ./ds4-smart-proxy.toml
```

設定不整合は起動時に検出します。backend IDの重複、複数local backend、userinfoを
含むURL、無効なtimeout、短いaffinity secretなどは起動エラーになります。

旧版のflat設定（`self_name`、`heartbeat_interval`、backendの`name`など）は
新仕様では受理しません。exampleに従ってセクション形式へ移行してください。

## Routing

新規requestの基本順序は次のとおりです。

1. 有効な既存affinity
2. local backend
3. priorityの高いremote backend
4. in-flight比率
5. EWMA latency
6. backend ID

同じaffinityがbusyの場合、既定では短いgrace期間だけ待ち、別backendへ即座に
spillせず503を返します。長いprefixを別backendで再prefillすることを避けるためです。

backend slotはSemaphoreで取得し、response bodyのEOF、エラー、client切断まで保持します。
`GET /v1/models`、`HEAD`、設定されたheartbeat pathは推論slotを消費しません。

`Suspect` backendでは、`GET /v1/models`などの非推論requestを転送できますが、
その成功だけでは`Alive`へ昇格しません。`Alive`が1台もない状態で実推論requestが
到着すると、`Unknown`または`Suspect` backendへsingle-flightのhalf-open requestを
1件だけ送り、最初の正常なresponse body到着で`Alive`へ復帰させます。

## Affinity header

正式なheader：

```text
X-DS4-Affinity-Key
X-DS4-Conversation-Id
X-DS4-Prefix-Hash
```

互換header：

```text
X-Hermes-Session-Id
X-Hermes-Session-Key
X-Conversation-Id
Conversation-Id
Session-Id
X-Session-Id
```

Responses APIの`conversation`、`previous_response_id`も認識します。
raw IDは保存・ログ出力せず、source namespaceを含むHMAC-SHA-256で保存します。

### Hermes連携

Hermesがcustom LLM providerへsession headerを自動転送する保証はないため、
LLM request middlewareでrequestごとにheaderを追加します。概念例：

```python
def add_ds4_affinity_header(**kwargs):
    request = dict(kwargs["request"])
    session_id = str(kwargs.get("session_id") or "").strip()
    if not session_id:
        return None

    headers = dict(request.get("extra_headers") or {})
    headers["X-DS4-Affinity-Key"] = f"hermes-session:{session_id}"
    request["extra_headers"] = headers
    return {"request": request, "name": "ds4-affinity"}
```

実際の登録方法は使用中のHermes middleware APIに合わせてください。静的な
`extra_headers`へ固定session IDを書くと、全sessionが同じbackendへ固定されるため避けます。

## Timeoutとretry

- DNS、TCP、TLS接続失敗は、再生可能なrequestに限り別backendへ1回retry
- HTTP 502 / 503 / 504は、別のavailable backendがある場合だけ1回retry
- HTTP 4xxはretryしない
- response header timeoutとfirst body byte timeoutは重複推論防止のためretryしない
- downstreamへresponseを開始した後はretryしない
- 2回目は必ず異なるbackendを選択

response bodyはContent-Typeにかかわらずstreamingし、全体をbufferしません。
first body byteを受信してからdownstream responseをcommitするため、それ以前と
stream開始後のfailure境界が明確です。

## 管理API

管理APIは既定で `127.0.0.1:18081` にのみbindします。

| Endpoint | 内容 |
|---|---|
| `GET /healthz` | process liveness |
| `GET /readyz` | backendとaffinity storeのreadiness |
| `GET /backends` | backend状態、in-flight、EWMA |
| `GET /affinity` | 件数とbackend別集計 |
| `DELETE /affinity/{tag}` | 12桁hash tagまたは完全hashのentry削除 |
| `GET /metrics` | Prometheus text format |

admin listenerの非loopback bindは、認証なしで状態を公開しないため起動時に拒否します。

## セキュリティ

- backend TLS証明書検証は既定で有効
- `Authorization`は転送するがログには出さない
- request/response body、session ID、conversation ID、完全affinity hashをログへ出さない
- affinity keyはrouting hintであり、認証・認可には使用しない
- backend URLにuserinfoを含めない
- affinity SQLiteにはraw identifierを保存しない

詳細な要件と受け入れ条件は [`docs/spec.md`](docs/spec.md) を参照してください。
