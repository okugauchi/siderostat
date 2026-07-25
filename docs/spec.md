# DS4 Smart Proxy

## 1. 文書情報

### 1.1 目的

本仕様は、これまでの運用で判明した次の事実を反映する。

- DS4は1バックエンドあたり実質1推論を中心に扱う。
- GPU使用率は生成終了後も高止まりすることがあり、routing signalとして信頼しにくい。
- 定期的なinference probeはGPU負荷を発生させ、本末転倒になる。
- 定期監視は `GET /v1/models` の軽量heartbeatとする。
- active inference probeは実リクエスト到着時かつ状態不明時だけに限定する。
- 長いHermesセッションでは、単純なleast-busyよりsession/prefix affinityが重要である。
- streaming開始後の別backend retryは重複推論や出力破損を起こし得る。
- standalone DS4群だけでなく、DS4 distributed coordinatorを1つのbackendとして共存させる必要がある。

### 1.2 実装言語と主要技術

- Rust stable
- Tokio
- Axum
- Hyper / hyper-util
- Tower / tower-http
- Serde
- tracing

本ソフトウェアは通常のHTTP forward proxyではない。OpenAI互換APIサーバとして待ち受け、選択したDS4 backendへ転送する **reverse proxy / application gateway** である。

### 1.3 公開仕様の確認時点

2026年7月25日時点

- DS4 main commit：[`0a7ad776b9068348e6cb09df8cafa9cadd285298`](https://github.com/antirez/ds4/commit/0a7ad776b9068348e6cb09df8cafa9cadd285298)
- Hermes Agent main commit：[`ca3566301373d871f05c1841dafe11cbd8e37a4d`](https://github.com/NousResearch/hermes-agent/commit/ca3566301373d871f05c1841dafe11cbd8e37a4d)
- Hermes API Server docs：[API Server](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/api-server.md)
- Hermes programmatic integration：[Programmatic Integration](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/programmatic-integration.md)

Hermesの公開仕様では `X-Hermes-Session-Id` と `X-Hermes-Session-Key` が定義されている。ただし、これらはHermes API Serverへ入るリクエストの受信側契約であり、Hermesがcustom LLM providerへ常に自動転送することは公開実装から確認できない。本仕様ではこの不確実性を明示し、Hermes integration middlewareまたは呼び出し側でrouting headerを付与するものとする。

## 2. 概要

`ds4-smart-proxy` は、複数のOpenAI互換DS4 endpointから適切なbackendを選び、HTTP request/responseを透過的に中継する。

典型構成：

```text
Codex / Hermes / OpenAI SDK / curl
              |
              v
http://127.0.0.1:18080/v1/...
              |
              v
       ds4-smart-proxy
          /       \
         /         \
standalone DS4   standalone DS4
または
distributed DS4 coordinator
```

各Macで同一バイナリを実行し、設定ファイルだけを変えられること。

## 3. Goals

- OpenAI互換APIの透過中継
- local-first routing
- session/prefix affinity
- streaming/SSEの低遅延中継
- backendごとの `max_in_flight`
- 軽量heartbeat
- 明確なtimeout、retry、cooldown
- standalone/distributed backendの共存
- 単一実行ファイル
- 外部状態サービスなしでも動作
- 構造化ログ
- Prometheus互換metrics
- macOS上で低オーバーヘッド

## 4. Non-goals

- 分散推論そのもの
- GPU kernel scheduling
- request batching
- tokenizerの完全再実装
- DS4内部KV cache keyの再現
- OAuth/OIDC identity provider
- 課金
- multi-tenant quota
- Kubernetes service discovery
- backend間で進行中推論を移送すること
- streaming開始後のtransparent failover

## 5. 設計原則

1. **実リクエスト成功が最良のhealth signalである。**
2. **定期監視でinferenceを実行しない。**
3. **Affinityは負荷分散より優先するが、障害時は安全に解除できる。**
4. **GPU使用率、CPU使用率、ユニファイドメモリ使用率は初期routingに使わない。**
5. **リクエストを二重実行しない。**
6. **streamingはbufferしない。**
7. **秘密値・session ID・prompt本文をログへ出さない。**
8. **distributed coordinatorは1つの論理backendとして扱う。**
9. **未知のOpenAI互換pathも転送する。**
10. **設定不整合は起動時にfail fastする。**

## 6. 用語

| 用語 | 定義 |
|---|---|
| Backend | proxyがリクエストを転送できる1つのHTTP(S) endpoint |
| Standalone backend | 単独のDS4 server |
| Distributed backend | DS4 distributed構成のcoordinator API endpoint |
| Local backend | proxyと同じマシン上にある優先backend |
| Affinity key | 同一会話・prefixを同じbackendへ送るための安定識別子 |
| Affinity entry | affinity key hashとbackend IDの対応 |
| Heartbeat | `GET /v1/models` による非推論生存確認 |
| Active probe | 小さなcompletionを実行する推論確認 |
| In-flight | proxyがbackendへ転送し、response body完了まで保持しているrequest |
| Suspect | HTTPは生きていても推論の健全性に疑いがある状態 |
| Cooldown | 一定時間routing対象から外す状態 |
| Prefix hash | routing affinity用のhint。DS4内部Disk KVのSHA-1とは別物 |

## 7. 全体アーキテクチャ

### 7.1 コンポーネント

```text
HTTP Listener (Axum)
  |
  +-- Request ID / tracing middleware
  +-- Authentication pass-through / redaction
  +-- Affinity key extractor
  +-- Router
  |     +-- Backend registry
  |     +-- Affinity store
  |     +-- Health state
  |     +-- Admission / in-flight guard
  |
  +-- Streaming upstream client
  |
  +-- Admin endpoints
  +-- Metrics

Background tasks
  +-- /v1/models heartbeat
  +-- affinity expiry
  +-- persistence flush
  +-- cooldown recovery eligibility
```

### 7.2 データフロー

```text
1. request受信
2. request ID生成/受理
3. affinity key抽出
4. 既存affinity entry検索
5. 候補backendのhealth/busy/cooldown判定
6. backend予約（in_flight increment）
7. upstreamへrequest転送
8. response header/bodyをstream
9. body完了・切断・errorでRAII guard drop
10. state/metrics/affinity last_seen更新
```

## 8. HTTP互換性

### 8.1 転送対象

最低限：

```text
GET  /v1/models
POST /v1/chat/completions
POST /v1/responses
POST /v1/completions
POST /v1/messages
```

未知pathも原則そのまま転送する。

proxy自身の管理pathは予約する。

```text
GET /healthz
GET /readyz
GET /backends
GET /metrics
GET /affinity
DELETE /affinity/{key_hash}
```

管理pathのprefixは設定可能にしてもよい。初期値は上記とする。

### 8.2 Method、query、body

- HTTP methodを保持する。
- query stringを保持する。
- request bodyを通常は変更しない。
- affinity抽出のためbodyを読む場合は、設定された最大サイズ以下で1回だけ読み、同じbytesをupstreamへ送る。
- `Content-Type` を保持する。
- `Content-Length` はbodyを再構成した場合のみ再計算する。

### 8.3 Header

hop-by-hop headerは転送しない。

```text
Connection
Keep-Alive
Proxy-Authenticate
Proxy-Authorization
TE
Trailer
Transfer-Encoding
Upgrade
```

次を適切に設定する。

```text
X-Forwarded-For
X-Forwarded-Host
X-Forwarded-Proto
X-Request-Id
Via
```

`Authorization` はupstreamへ転送できること。ただしログでは必ずredactする。

### 8.4 Response

- status codeを保持する。
- end-to-end headerを保持する。
- hop-by-hop headerを除外する。
- streaming bodyをbufferしない。
- client disconnectをupstream body cancellationへ伝播する。

### 8.5 非推論request

`GET /v1/models`、`HEAD`、および明示設定されたhealth pathは、推論用
`max_in_flight` slotを消費しない。これらはHTTP生存確認であり、DS4の推論worker占有を
表さないためである。

ただし、非推論requestを同時無制限に流してよいという意味ではない。別の小さな
concurrency limitとtimeoutを適用する。

## 9. Streaming

### 9.1 要件

- SSE eventを集約しない。
- chunk順序を保持する。
- upstreamの最初のbody chunkを受け次第clientへ流す。
- backpressureはTokio/Hyperへ委ねる。
- body全体をメモリへ保持しない。
- stream終了までbackendをin-flightとして数える。

### 9.2 Streaming開始判定

次のいずれかを満たした時点で「clientへresponse開始済み」とする。

- response headerをclientへcommitした。
- 最初のbody chunkを書き出した。

この時点以降は別backendへretryしてはならない。

## 10. Backend設定

### 10.1 Backend種別

```rust
enum BackendKind {
    Standalone,
    Distributed,
}
```

Distributed backendは、複数のunderlying Macをproxyが直接操作する意味ではない。DS4 coordinatorの1 endpointを1 backendとして登録する。

### 10.2 Backend構成

```rust
struct BackendConfig {
    id: String,
    url: Url,
    kind: BackendKind,
    local: bool,
    enabled: bool,
    priority: i32,
    max_in_flight: usize,
    heartbeat_path: String,
    tls: TlsConfig,
    static_headers: HeaderMap,
    tags: Vec<String>,
}
```

### 10.3 制約

- `id` は全backendで一意。
- local backendは0または1件。local-first modeでは1件を推奨。
- `max_in_flight >= 1`。
- backend URLにuserinfoを含めない。
- URLのpath末尾は正規化する。
- distributed backendの `max_in_flight` 初期値は1。

## 11. Backend runtime state

```rust
enum BackendHealth {
    Unknown,
    Alive,
    Suspect,
    Offline,
    Cooldown,
    Disabled,
}

struct BackendState {
    health: BackendHealth,
    in_flight: usize,
    ewma_latency_ms: Option<f64>,
    last_heartbeat_at: Option<Instant>,
    last_heartbeat_ok_at: Option<Instant>,
    last_success_at: Option<Instant>,
    last_failure_at: Option<Instant>,
    last_failure_kind: Option<FailureKind>,
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
}
```

### 11.1 Available

Backendは次をすべて満たすとrouting候補になる。

```text
enabled
health ∈ {Alive}
cooldown_until <= now
in_flight < max_in_flight
```

`Unknown` / `Suspect` はactive probe policyにより候補復帰を試せる。

### 11.2 Busy

```text
busy = in_flight >= max_in_flight
```

GPU usageはbusy判定に使わない。

### 11.3 RAII admission guard

Backend選択と同時にatomicにslotを取得する。

```rust
struct InFlightGuard {
    backend: Arc<BackendRuntime>,
    released: bool,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.released {
            self.backend.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
```

選択とincrementを別操作にしない。複数requestが同時に同じ最後のslotを取得しないよう、compare-and-swap loopまたはSemaphore permitを使う。

Tokio `Semaphore` のowned permitをguardとして保持する実装を推奨する。

## 12. Heartbeat

### 12.1 方針

定期heartbeatは次だけを実行する。

```text
GET {backend}/v1/models
```

inferenceを定期実行してはならない。

### 12.2 初期値

```text
interval: 10s
timeout:  3s
jitter:   ±20%
```

全backendを同時刻に叩かない。

### 12.3 状態遷移

```text
Unknown + success  -> Alive
Alive   + success  -> Alive
Alive   + failure  -> 失敗閾値まではAlive、超過でOffline
Suspect + success  -> Suspectのまま（HTTP生存だけでは推論復旧としない）
Offline + success  -> Suspect
Cooldown expiry    -> Suspect
```

`Suspect` から `Alive` へ戻すには、次のいずれかを要求する。

- 実client requestが成功
- rate-limited active probeが成功
- 管理APIによる明示復帰

### 12.4 Log noise

heartbeat成功を毎回INFOへ出さない。state changeだけINFO、個々の結果はDEBUGまたはmetricsとする。

## 13. Active inference probe

### 13.1 原則

Active probeは高価である。

- timerで実行しない。
- backendがbusyなら実行しない。
- 同じbackendへ同時に複数probeしない。
- 実リクエストのslotを奪わない。
- 既にrecent successがあるbackendへ実行しない。

### 13.2 実行条件

実client requestが到着し、候補backendが `Unknown` または `Suspect` で、他に `Alive && available` なbackendがない場合に限り実行できる。

### 13.3 Probe request

```http
POST /v1/chat/completions
Content-Type: application/json
```

```json
{
  "model": "deepseek-v4-flash",
  "messages": [
    {
      "role": "user",
      "content": "Reply with exactly OK."
    }
  ],
  "reasoning_effort": "none",
  "temperature": 0,
  "max_tokens": 4,
  "stream": false
}
```

### 13.4 初期値

```text
enabled: false
timeout: 5s
minimum_interval_per_backend: 300s
```

既定は無効とする。DS4の長いprefill中にprobeを送ると、worker queueへ追加負荷をかけるためである。

## 14. Routing policy

### 14.1 優先順位

基本順序：

1. valid affinity entryが指すavailable backend
2. affinity keyがある場合の新規割当
3. local backend
4. priorityの高いremote backend
5. least in-flight ratio
6. EWMA latency
7. stable backend ID

### 14.2 local-first

affinity entryがない新規requestではlocal backendを優先する。

```text
local && available
```

既存affinityがremoteを指す場合、local-firstよりaffinityを優先する。KV/prefix localityを守るためである。

### 14.3 同点

```text
priority降順
in_flight / max_in_flight 昇順
ewma_latency昇順
backend ID辞書順
```

ランダム選択は再現性を損なうため、初期実装では行わない。

### 14.4 候補なし

初期実装ではqueueを持たず、次を返す。

```http
HTTP/1.1 503 Service Unavailable
Retry-After: 5
Content-Type: application/json
```

```json
{
  "error": {
    "message": "No DS4 backend is currently available",
    "type": "service_unavailable",
    "code": "no_backend_available"
  }
}
```

## 15. Sticky routing / Affinity

### 15.1 目的

同じHermes transcript、conversation、またはstable prefixを、可能な限り同じDS4 backendへ送る。

これにより次を期待する。

- live KV cacheの再利用
- Disk KV cache locality
- 長いsystem prompt、skill、tool historyの再prefill削減
- backend切替時の600秒級tail latency削減
- compaction直前後のcache locality維持

Affinityは「同じ意味のタスク」を理解する仕組みではない。明示IDまたは安定prefixから同一性を判断する。

### 15.2 正式なproxy header contract

`ds4-smart-proxy` が正式に受理するheader：

```text
X-DS4-Affinity-Key
X-DS4-Conversation-Id
X-DS4-Prefix-Hash
```

互換入力として次も認識する。

```text
X-Hermes-Session-Id
X-Hermes-Session-Key
X-Conversation-Id
Conversation-Id
Session-Id
X-Session-Id
```

header名はcase-insensitive。値はcase-sensitiveとして扱う。

### 15.3 キー抽出優先順位

最初に見つかったvalid valueを使用する。

1. `X-DS4-Affinity-Key`
2. `X-Hermes-Session-Id`
3. `X-Hermes-Session-Key`
4. `X-DS4-Conversation-Id`
5. `X-Conversation-Id`
6. `Conversation-Id`
7. `Session-Id`
8. `X-Session-Id`
9. Responses API bodyの `conversation` ID
10. Responses API bodyの `previous_response_id`
11. request bodyの明示 `session_id` / `conversation_id`（設定で許可した場合）
12. `X-DS4-Prefix-Hash`
13. proxyによるprefix fingerprint計算（設定で有効な場合）
14. affinityなし

`X-Hermes-Session-Id` はtranscript単位で変化し得る。`X-Hermes-Session-Key` はchannel/long-term memory scopeとしてより長寿命である。両方ある場合はtranscript localityを優先してSession-Idを使う。

### 15.4 Hermesに関する確定事項と不確定事項

確定：

- Hermes API Serverは `X-Hermes-Session-Id` をtranscript continuityに使う。
- Hermes API Serverは `X-Hermes-Session-Key` を長期memory scopeに使う。
- `X-Hermes-Session-Key` は最大256文字で、control characterを拒否する。
- HermesにはLLM request middlewareがあり、provider request kwargsの `extra_headers` を動的に変更できる。

不確定：

- Hermes gateway/custom providerが上記headerをLLM providerへ標準で自動転送する保証はない。
- Hermesの将来バージョンで内部session構造やheader伝播が変わる可能性がある。

したがって、Hermes側で次のいずれかを実装する必要がある。

1. LLM request middlewareで、middleware contextの `session_id` を `X-DS4-Affinity-Key` として `extra_headers` に追加する。
2. Hermes本体へprovider affinity header機能を追加する。
3. 外側の呼び出し元がheaderを付ける。

静的 `extra_headers` 設定へ固定session IDを書く方法は、全sessionが同じbackendへ固定されるため不可。

### 15.5 Hermes middleware擬似コード

Python概念例：

```python
def add_ds4_affinity_header(**kwargs):
    request = dict(kwargs["request"])
    session_id = str(kwargs.get("session_id") or "").strip()
    if not session_id:
        return None

    headers = dict(request.get("extra_headers") or {})
    headers["X-DS4-Affinity-Key"] = f"hermes-session:{session_id}"
    request["extra_headers"] = headers

    return {
        "request": request,
        "name": "ds4-affinity",
    }
```

これはHermes plugin/middleware APIの実際の登録方法に合わせて実装する。`session_id` をログへ出さない。

### 15.6 値のvalidation

入力値は次を満たす必要がある。

- UTF-8として扱える
- 1〜256 bytes
- `\r`、`\n`、NUL、その他C0/C1 control characterを含まない
- leading/trailing ASCII whitespaceはtrim
- UnicodeはNFC normalize
- 空文字は禁止

invalid headerが最優先位置にある場合：

- `X-DS4-*` のinvalid値はHTTP 400
- 互換headerのinvalid値は無視し、次候補へ進む

### 15.7 Namespace

異なるsourceの同じ文字列が衝突しないよう、source名をnamespaceに含める。

例：

```text
explicit:abc
hermes-session:abc
hermes-key:abc
conversation:abc
body-previous-response:resp_abc
prefix-sha256:abc
```

### 15.8 保存用ハッシュ

生値を保存しない。

```text
key_hash = SHA-256(
  affinity_secret
  || 0x00
  || source_namespace
  || 0x00
  || normalized_value
)
```

`affinity_secret` は設定ファイルまたは環境変数から読み込む。HMAC-SHA-256を使う実装でもよい。推奨はHMAC-SHA-256。

```text
key_hash = HMAC-SHA-256(secret, namespace || 0x00 || value)
```

ログには先頭12 hex文字だけを `affinity_key_tag` として出す。完全hashも通常ログへ出さない。

### 15.9 Prefix hash

#### Caller-provided

`X-DS4-Prefix-Hash` は次を要求する。

- lowercase/uppercase hexまたはbase64url
- decode後16〜64 bytes
- prompt本文そのものを含まない

#### Proxy-computed

設定 `compute_prefix_affinity = true` の場合だけ計算する。

Chat Completions：

```text
model
+ 最初のsystem/developer message
+ 最初のuser message
+ tool schemaの安定部分
```

Responses API：

```text
model
+ instructions
+ inputの先頭安定要素
```

正規化：

- JSON object keyを辞書順
- insignificant whitespaceを除去
- number表現をcanonical化
- volatile fieldを除外
- 最大bytesまでで打ち切り

```text
prefix_fingerprint = SHA-256(canonical_prefix_bytes)
```

注意：

- これはrouting hintである。
- DS4がrenderしたprompt bytesやtoken IDsとは一致しない。
- DS4 Disk KV cacheの内部SHA-1 keyを再現しない。
- false positiveを避けるため、短すぎるprefixでは使用しない。

初期値：

```text
compute_prefix_affinity = false
minimum_prefix_bytes = 4096
maximum_prefix_hash_bytes = 1048576
```

### 15.10 Affinity entry

```rust
struct AffinityEntry {
    key_hash: [u8; 32],
    source: AffinitySource,
    backend_id: String,
    created_at: SystemTime,
    last_seen_at: SystemTime,
    expires_at: SystemTime,
    absolute_expires_at: SystemTime,
    assignment_generation: u64,
    failure_count: u32,
}
```

### 15.11 TTL

初期値：

| source | sliding TTL | absolute TTL |
|---|---:|---:|
| explicit `X-DS4-Affinity-Key` | 7日 | 30日 |
| `X-Hermes-Session-Id` | 7日 | 30日 |
| `X-Hermes-Session-Key` | 14日 | 90日 |
| conversation ID | 7日 | 30日 |
| previous_response_id | 24時間 | 7日 |
| prefix hash | 24時間 | 7日 |

request受信時に `last_seen_at` とsliding expiryを更新する。ただしabsolute TTLを超えない。

### 15.12 Persistence

#### 要件

- proxy再起動後もaffinityを維持する。
- raw IDを保存しない。
- crash後に破損しにくい。
- 同一proxy processだけがDB writerである。

#### 推奨実装

SQLite WALを使用する。Rustでは `rusqlite` を候補とする。

```sql
CREATE TABLE affinity (
    key_hash BLOB PRIMARY KEY,
    source INTEGER NOT NULL,
    backend_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    absolute_expires_at INTEGER NOT NULL,
    assignment_generation INTEGER NOT NULL,
    failure_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX affinity_expires_at_idx
ON affinity(expires_at);
```

WAL：

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
```

書込みはrequest pathを長くblockしないよう、in-memory mapを主、bounded async channel経由でSQLiteへ反映してよい。

永続化失敗時：

- routing自体はin-memoryで継続。
- healthをdegradedとして `/readyz` またはmetricsへ反映。
- raw affinity valueはログへ出さない。

### 15.13 新規割当

```text
affinity keyあり
既存entryなし
```

の場合、通常routing policyでbackendを選び、upstreamへの接続開始が成功した時点でentryを作る。

request受信直後に作らない。slot取得や接続が失敗したbackendへ誤ってpinしないためである。

実requestがHTTP 2xx/3xxまたはstream開始まで到達したらentryを確定する。4xxはbackend障害ではないが、そのbackendでrequestが処理されたためentryを維持してよい。

### 15.14 再割当条件

既存entryが次のいずれかなら再割当可能。

- backendが設定から削除された
- backendがDisabled
- heartbeat失敗閾値を超えOffline
- backendがCooldown
- connection failureが発生し、response未開始
- TTL expired
- 管理APIでentryを削除
- operatorがbackend drainを指定

Busyだけを理由に即座に再割当しない。短い待機または503を返す方がKV localityを守れる場合がある。

設定：

```text
affinity_busy_policy = "fail" | "spill"
```

初期値：

```text
affinity_busy_policy = "fail"
affinity_busy_grace = 2s
```

`fail` はgraceの間だけslot解放を待ち、取得できなければ503を返す。長いprefixの
再prefillより、callerに短時間後のretryを促す方を既定とする。

`spill` の場合は別backendへ割り当て、成功した時点でentryを更新する。

長期Hermes workloadでは `fail` または短いqueueを選択できるようにする。初期MVPではqueueを実装せず、`fail` は503を返す。

### 15.15 Failure時

#### Connection failure before response

- 元backendをSuspect/Cooldownへ移す。
- affinity entryのfailure_countを増やす。
- 別backendへretry可能。
- retry成功時にentryを新backendへ更新。

#### HTTP 5xx before response body

- retry policyが許可すれば別backendへ1回だけretry。
- 元backendをSuspectにする。

#### HTTP 4xx

- retryしない。
- affinityを維持。
- backend healthを悪化させない。

#### First-byte timeout

DS4はclient切断後も処理を続ける可能性があるため、既定では自動retryしない。

- clientへ504
- backendをCooldown
- affinity entryは失敗回数を増やす
- 次のclient retry時に別backendへ再割当可能

#### Streaming開始後

- retry禁止
- downstream connectionを閉じる
- partial responseをエラーとして記録
- affinityは維持するがbackendをSuspectにできる

### 15.16 複数proxy instance

各Macにproxyを置く構成では、affinity storeは原則ローカルであり、同じkeyが別proxyで異なるbackendへ割り当てられる可能性がある。

初期仕様はこれを許容する。Hermes gatewayが通常1つのlocal proxyを継続利用する限り、session affinityは成立する。

複数proxy間の厳密な一貫性が必要になった場合の拡張候補：

- identical backend IDsとsecretによるRendezvous hashing
- 共有SQLiteは不可
- 小さなcentral lease service
- backend側admission endpoint

MVPで分散consensusやRedisを導入しない。

## 16. Distributed backendとの共存

### 16.1 モデル

DS4 distributed coordinatorは1つのbackendとして登録する。

```toml
[[backends]]
id = "distributed-main"
kind = "distributed"
url = "http://127.0.0.1:8100"
local = true
priority = 100
max_in_flight = 1
```

underlying workerのhealthはDS4 coordinatorが管理する。proxyはcoordinatorの `/v1/models` と実request結果を見る。

### 16.2 Standalone fallback

```toml
[[backends]]
id = "standalone-macstudio"
kind = "standalone"
url = "http://127.0.0.1:8000"
priority = 50
max_in_flight = 1
```

ただし同じMac上でdistributedとstandaloneのモデルを同時residentにできない構成では、両方を同時enabledにしない。

### 16.3 Affinity

Affinity entryはbackend kindに関係なくbackend IDを指す。

Distributed backendへpinされたsessionは、worker切断によりcoordinatorがincomplete routeになった場合、次のrequestでstandaloneへ再割当できる。

途中のrequestをdistributedからstandaloneへtransparentに移送しない。

### 16.4 Routing profile

将来拡張として次を設定可能にする。

```toml
[routing]
mode = "local-first"
prefer_distributed_for_affinity = false
```

`prefer_distributed_for_affinity = true` の場合、affinity keyを持つ長期sessionの新規割当ではdistributed backendをlocal standaloneより優先できる。

MVP既定は `false` とし、local-firstを維持する。

## 17. Timeout

### 17.1 種別

```rust
struct TimeoutConfig {
    connect: Duration,
    response_headers: Duration,
    first_body_byte: Duration,
    stream_idle: Duration,
    total: Option<Duration>,
}
```

### 17.2 初期値

```text
connect:          5s
response_headers: 60s
first_body_byte:  300s
stream_idle:      300s
total:            none
```

DS4の長いprefillは正常でもTTFTが長くなるため、値は設定可能にする。

### 17.3 意味

- connect：TCP/TLS接続完了まで
- response_headers：upstream response headerまで
- first_body_byte：response header後、最初のbody chunkまで
- stream_idle：stream開始後、次chunkまで
- total：request全体。既定では無制限

## 18. Retry

### 18.1 基本

最大attempt：

```text
2
```

2回目は必ず異なるbackend。

### 18.2 Retry可能

- DNS/connect/TLS failure
- upstreamへbody送信前のconnection failure
- response未開始のHTTP 502/503/504
- 明示的backend admission failure

### 18.3 Retry禁止

- clientへresponse header送信後
- body chunk送信後
- HTTP 4xx
- request bodyを安全に再生できない
- first-byte timeout（既定）
- client disconnect

### 18.4 Request body replay

retryを可能にするため、上限以下のrequest bodyをbytesとして保持する。

```text
max_replayable_body_bytes = 32 MiB
```

上限超過bodyはstream passthroughし、retry不可とする。

## 19. Cooldown / Circuit breaker

### 19.1 Failure分類

```rust
enum FailureKind {
    Connect,
    Tls,
    Heartbeat,
    ResponseHeaderTimeout,
    FirstByteTimeout,
    StreamIdleTimeout,
    Http5xx,
    Protocol,
    ClientCancelled,
}
```

`ClientCancelled` はbackend failure countへ加えない。

### 19.2 初期値

```text
consecutive_failure_threshold = 2
cooldown = 300s
half-open probe interval = 300s
```

first-byte timeoutは1回でCooldownにしてよい。

### 19.3 復帰

Cooldown終了後はSuspectへ遷移する。

- 別のAlive backendがあれば、通常requestには使わない。
- 他に候補がない時だけactive probeまたは明示operator actionで復帰。
- 実request成功でAlive。

## 20. Configuration

### 20.1 ファイル

TOML。

探索順：

1. CLI `--config`
2. `DS4_SMART_PROXY_CONFIG`
3. `./ds4-smart-proxy.toml`
4. platform default

### 20.2 完全例

```toml
listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"
request_body_limit_bytes = 33554432
max_replayable_body_bytes = 33554432

[routing]
mode = "local-first"
affinity_busy_policy = "fail"
affinity_busy_grace = "2s"
prefer_distributed_for_affinity = false
max_attempts = 2

[heartbeat]
interval = "10s"
timeout = "3s"
failure_threshold = 2
jitter_ratio = 0.20

[active_probe]
enabled = false
timeout = "5s"
minimum_interval = "300s"
model = "deepseek-v4-flash"

[timeouts]
connect = "5s"
response_headers = "60s"
first_body_byte = "300s"
stream_idle = "300s"

[cooldown]
duration = "300s"
consecutive_failure_threshold = 2

[affinity]
enabled = true
secret_env = "DS4_SMART_PROXY_AFFINITY_SECRET"
database_path = "/Users/USER/Library/Application Support/ds4-smart-proxy/affinity.sqlite3"
compute_prefix_affinity = false
minimum_prefix_bytes = 4096
maximum_prefix_hash_bytes = 1048576
default_sliding_ttl = "7d"
default_absolute_ttl = "30d"

[logging]
format = "json"
level = "info"
redact_headers = [
  "authorization",
  "proxy-authorization",
  "x-api-key",
  "x-ds4-affinity-key",
  "x-hermes-session-id",
  "x-hermes-session-key"
]

[[backends]]
id = "macbook-local"
kind = "standalone"
url = "http://127.0.0.1:8000"
local = true
enabled = true
priority = 100
max_in_flight = 1
heartbeat_path = "/v1/models"

[[backends]]
id = "macstudio-remote"
kind = "standalone"
url = "https://macstudio-ds4.home.arpa"
local = false
enabled = true
priority = 50
max_in_flight = 1
heartbeat_path = "/v1/models"
```

### 20.3 Secret

```bash
export DS4_SMART_PROXY_AFFINITY_SECRET='32 bytes以上のランダム値'
```

secretが未設定でaffinity persistenceが有効なら起動失敗する。raw IDをplain SHA-256だけで保存しない。

secretを変更すると既存DBのkey hashを再計算できないため、旧entryは利用不能になる。
secret rotation時は旧DBを退避して新規DBを開始するか、複数世代secretを扱う明示的な
migration機能を実装する。

## 21. Admin API

Admin APIは初期値でloopbackだけにbindする。

### 21.1 `/healthz`

processが生きてevent loopが応答できれば200。

```text
OK
```

### 21.2 `/readyz`

少なくとも1 backendがAlive/available、設定とaffinity storeが利用可能なら200。それ以外は503。

### 21.3 `/backends`

```json
[
  {
    "id": "macbook-local",
    "kind": "standalone",
    "local": true,
    "health": "alive",
    "in_flight": 0,
    "max_in_flight": 1,
    "ewma_latency_ms": 85.2,
    "last_success_at": "2026-07-25T10:00:00Z",
    "cooldown_until": null
  }
]
```

秘密header、URL credential、affinity raw valueを返さない。

### 21.4 `/affinity`

管理用。既定で件数・backend別集計だけを返す。

```json
{
  "entries": 42,
  "by_backend": {
    "macbook-local": 20,
    "macstudio-remote": 22
  }
}
```

個別key lookupはhash tagだけを受け付ける。raw session ID検索を提供しない。

## 22. Logging

### 22.1 Request log fields

```text
timestamp
level
request_id
method
path_templateまたはredacted path
backend_id
backend_kind
routing_reason
affinity_source
affinity_key_tag
affinity_hit
in_flight_before
attempt
upstream_connect_ms
response_header_ms
first_body_byte_ms
total_ms
status
bytes_in
bytes_out
error_kind
```

### 22.2 Routing reason

列挙型：

```text
affinity_hit
affinity_reassigned
local_first
remote_fallback
distributed_preferred
least_loaded
only_available
retry_different_backend
```

### 22.3 Redaction

ログ禁止：

- Authorization
- API key
- Cookie
- request/response body
- session ID raw value
- conversation ID raw value
- prefix本文
- full affinity hash

### 22.4 TTFB

以下を分けて記録する。

- upstream connect
- response headers
- first body byte
- stream completion

Hermesの「600秒無出力」の原因層を特定できること。

## 23. Metrics

Prometheus text format。

最低限：

```text
ds4_proxy_requests_total{backend,status_class}
ds4_proxy_request_duration_seconds
ds4_proxy_time_to_first_byte_seconds
ds4_proxy_in_flight{backend}
ds4_proxy_backend_health{backend,state}
ds4_proxy_heartbeat_total{backend,result}
ds4_proxy_retries_total{from_backend,to_backend,reason}
ds4_proxy_cooldown_total{backend,reason}
ds4_proxy_affinity_lookup_total{source,result}
ds4_proxy_affinity_entries{backend}
ds4_proxy_active_probe_total{backend,result}
ds4_proxy_stream_errors_total{backend,reason}
```

session IDやaffinity hashをlabelにしない。cardinality爆発と情報漏えいを防ぐ。

## 24. Error response

OpenAI風JSONを返す。

```json
{
  "error": {
    "message": "Upstream DS4 backend timed out before producing output",
    "type": "upstream_timeout",
    "code": "first_body_byte_timeout",
    "request_id": "req_..."
  }
}
```

Mapping：

| 状況 | Status |
|---|---:|
| invalid affinity header | 400 |
| request body too large | 413 |
| no backend available | 503 |
| connect failure after retries | 502 |
| response header/first byte timeout | 504 |
| internal state error | 500 |

upstream bodyを受け取った4xx/5xxは、retryしない場合そのまま返す。

## 25. Security

### 25.1 Network

- 既定bindはloopback。
- LAN公開は明示設定。
- TLS終端はCaddy等へ委ねてもよい。
- backend TLS証明書検証を既定で有効。
- `danger_accept_invalid_certs` は既定false。

### 25.2 Header trust

LANへ公開する場合、外部clientが任意のaffinity keyを送れる。これはrouting hintであり認可には使わない。

DoS対策：

- header長制限
- body長制限
- affinity entry総数制限
- source別TTL
- LRU/expiry
- per-client rate limitは将来拡張

### 25.3 Affinity poisoning

攻撃者が他人のsession IDを知ってもraw IDからbackendを推測できないようHMACで保存する。

Affinity keyはauthentication tokenではない。権限判定へ使用しない。

### 25.4 Admin API

- loopback限定を既定。
- LAN公開時は別Bearer tokenを要求。
- backend static headerを返さない。

## 26. Concurrency

### 26.1 Shared state

```text
Backend registry: Arc<Vec<BackendRuntime>>
Backend state: atomics + RwLock
Affinity cache: DashMapまたはRwLock<HashMap>
Persistence writer: bounded mpsc channel
```

DashMap採用は必須ではない。backend数が少ないため、単純で正しい実装を優先する。

### 26.2 Lock

- network await中にglobal lockを保持しない。
- affinity DB I/Oをrequest routing lock内で行わない。
- backend slot acquisitionはatomic/semaphore。
- lock orderingを文書化する。

## 27. 推奨crate

必須候補：

- `tokio`
- `axum`
- `hyper`
- `hyper-util`
- `tower`
- `tower-http`
- `reqwest` またはHyper client
- `serde`
- `serde_json`
- `toml`
- `tracing`
- `tracing-subscriber`
- `thiserror`
- `anyhow`
- `clap`
- `uuid`
- `url`
- `sha2`
- `hmac`
- `unicode-normalization`
- `rusqlite`

依存追加は実装前に既存Cargo.tomlと重複・featureを確認する。

## 28. Module構成案

```text
src/
  main.rs
  config.rs
  error.rs
  http/
    mod.rs
    proxy.rs
    admin.rs
    headers.rs
    streaming.rs
  backend/
    mod.rs
    registry.rs
    state.rs
    heartbeat.rs
    probe.rs
    admission.rs
  routing/
    mod.rs
    policy.rs
    affinity.rs
    prefix.rs
    retry.rs
  persistence/
    mod.rs
    sqlite.rs
  observability/
    mod.rs
    logging.rs
    metrics.rs
```

## 29. Routing擬似コード

```rust
async fn handle(req: Request<Body>) -> Result<Response<Body>, ProxyError> {
    let request_id = request_id(&req);
    let replayable = read_or_stream_body(req).await?;
    let affinity = extract_affinity(&replayable)?;

    let mut excluded = HashSet::new();

    for attempt in 1..=config.routing.max_attempts {
        let selection = router
            .select(SelectInput {
                affinity: affinity.as_ref(),
                excluded: &excluded,
                prefer_local: true,
            })
            .await?;

        let permit = selection.backend.try_acquire().ok_or_else(|| {
            ProxyError::BackendBecameBusy(selection.backend.id.clone())
        })?;

        let result = forward(
            &selection.backend,
            &replayable,
            &request_id,
        )
        .await;

        match result {
            Ok(response) => {
                affinity_store.confirm_or_refresh(
                    affinity.as_ref(),
                    &selection.backend.id,
                ).await;

                return Ok(wrap_stream_with_permit(response, permit));
            }

            Err(err) if retry_policy.can_retry(&err, attempt, &replayable) => {
                backend_state.record_failure(&selection.backend.id, &err);
                excluded.insert(selection.backend.id.clone());
                drop(permit);
                continue;
            }

            Err(err) => {
                backend_state.record_failure(&selection.backend.id, &err);
                drop(permit);
                return Err(err);
            }
        }
    }

    Err(ProxyError::NoBackendAvailable)
}
```

重要：streaming responseのpermitは、handler return時ではなくresponse body drop/EOF時に解放する。

## 30. Affinity選択擬似コード

```rust
fn select_backend(
    key: Option<&AffinityKey>,
    backends: &[BackendRuntime],
    store: &AffinityStore,
) -> Result<Selection, SelectError> {
    if let Some(key) = key {
        if let Some(entry) = store.get(key.hash()) {
            if let Some(backend) = backends.by_id(&entry.backend_id) {
                if backend.is_available() {
                    return Ok(Selection::new(backend, "affinity_hit"));
                }
            }
        }
    }

    if let Some(local) = backends
        .iter()
        .filter(|b| b.config.local)
        .filter(|b| b.is_available())
        .next()
    {
        return Ok(Selection::new(local, "local_first"));
    }

    backends
        .iter()
        .filter(|b| b.is_available())
        .sorted_by(routing_order)
        .next()
        .map(|b| Selection::new(b, "remote_fallback"))
        .ok_or(SelectError::NoBackend)
}
```

## 31. Test strategy

### 31.1 Unit tests

- config parse/validation
- URL join
- hop-by-hop header除去
- affinity header優先順位
- Unicode NFC正規化
- control character拒否
- max length
- namespace衝突防止
- HMAC deterministic性
- TTL/sliding/absolute expiry
- routing order
- local-first
- affinity優先
- busy policy
- cooldown state transition
- retry predicate
- error mapping
- prefix canonicalization

### 31.2 Integration tests

Axum mock backendを2つ起動する。

1. local成功
2. local busy → remote
3. affinity remote → localが空いていてもremote
4. remote offline → affinity再割当
5. connect failure →別backend retry
6. 4xx no retry
7. 5xx before body → retry
8. SSE chunk timing保持
9. stream開始後切断 → no retry
10. client cancel → permit解放
11. heartbeatでstate transition
12. active probeがtimer実行されない
13. SQLite restart後affinity復元
14. expired entry削除
15. distributed backendを単一backendとして扱う

### 31.3 Concurrency tests

- `max_in_flight=1` に同時100request
- slot二重取得がない
- cancellation stormでin_flightが0へ戻る
- DB writer停止でもrouting継続
- heartbeatとroutingのrace
- cooldown expiry race

### 31.4 Streaming fidelity

mock upstreamが100ms間隔でSSEを送る。

- proxy downstreamも同等間隔で受信
- 全body bufferingがない
- chunk順序が同じ
- memory usageがresponse sizeに比例しない

### 31.5 Fault injection

- DNS failure
- TLS failure
- connection reset
- headers timeout
- first byte timeout
- mid-stream reset
- malformed chunk
- invalid JSON body
- oversized body
- SQLite disk full
- affinity DB corrupt

## 32. Acceptance criteria

### 32.1 Core

- [ ] Rust stableでbuildできる。
- [ ] macOS ARM64で動作する。
- [ ] `cargo fmt --check` 成功。
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 成功。
- [ ] `cargo test --all` 成功。
- [ ] `/v1/chat/completions` を透過中継できる。
- [ ] `/v1/responses` を透過中継できる。
- [ ] unknown pathを転送できる。
- [ ] SSEをbufferせず中継できる。

### 32.2 Health

- [ ] 定期処理は `/v1/models` だけ。
- [ ] timer active probeが存在しない。
- [ ] state changeだけINFO log。
- [ ] Suspectはheartbeat成功だけでAliveへ戻らない。

### 32.3 Routing

- [ ] affinityなしではlocal-first。
- [ ] existing affinityはlocal-firstより優先。
- [ ] max_in_flightを超えない。
- [ ] 候補なしは503。
- [ ] distributed coordinatorをbackend登録できる。

### 32.4 Affinity

- [ ] 指定優先順位でheader抽出。
- [ ] invalid explicit headerは400。
- [ ] raw IDを保存・ログしない。
- [ ] HMAC-SHA-256でkey化。
- [ ] TTLとabsolute TTLが動作。
- [ ] proxy再起動後もmappingを復元。
- [ ] backend障害時に再割当。
- [ ] streaming開始後は再割当retryしない。
- [ ] Hermes middleware統合方法がREADMEに記載される。

### 32.5 Timeout/retry

- [ ] connect/header/first-byte/stream-idleを個別設定。
- [ ] retryは最大2 attempt。
- [ ] 2回目は異なるbackend。
- [ ] 4xxはretryしない。
- [ ] first-byte timeoutは既定で自動retryしない。
- [ ] cooldown後はSuspectへ遷移。

### 32.6 Observability

- [ ] selected backendとrouting reasonを記録。
- [ ] TTFBを記録。
- [ ] request IDをresponseへ返す。
- [ ] secret/session ID/promptをログしない。
- [ ] `/metrics` が必要metricを返す。

### 32.7 Performance

- [ ] idle時CPU使用率が実用上無視できる。
- [ ] heartbeatがGPU inferenceを発生させない。
- [ ] 1つのstream転送でbody全体をbufferしない。
- [ ] proxy overheadのp50がLAN内で5ms未満を目標とする。

## 33. 実装フェーズ

### Phase 1

- config
- backend registry
- local-first
- max_in_flight
- heartbeat
- transparent streaming
- timeout
- structured logs
- `/healthz` `/backends`

### Phase 2

- affinity header抽出
- HMAC
- in-memory mapping
- TTL
- routing reason
- Hermes middleware example

### Phase 3

- SQLite persistence
- metrics
- cooldown
- safe retry
- distributed backend type

### Phase 4

- prefix fingerprint
- drain/admin
- optional queue
- multi-proxy deterministic routing

各Phase終了時にテストを追加し、後続機能のために未検証の抽象化を先行導入しない。

## 34. 未確定事項

次は公開仕様または実環境で確定していないため、実装時に仮定として扱う。

1. Hermes custom providerが将来 `X-Hermes-Session-Id` を自動転送するか。
2. Hermesのどのsession identifierが暗黙的後処理タスク間で最も安定するか。
3. compaction前後で同一Session-Idが維持されるか、Session-Keyだけが維持されるか。
4. DS4がclient disconnect後に全ケースで推論を停止するか。
5. DS4 distributed coordinatorのincomplete routeが `/v1/models` に反映されるか。
6. OpenAI Responses APIの `conversation` fieldの具体形が接続clientごとに同じか。
7. proxy-computed prefix fingerprintがDS4 cache localityを十分予測できるか。

これらはログとA/B運用で検証する。仮定を確定仕様としてコードへ埋め込まない。

## 35. 参考資料

- [DS4 GitHub repository](https://github.com/antirez/ds4)
- [DS4 distributed inference](https://github.com/antirez/ds4#distributed-inference-with-pipeline-parallelism)
- [Hermes Agent API Server](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/api-server.md)
- [Hermes Agent programmatic integration](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/programmatic-integration.md)
- [Hermes Agent middleware implementation](https://github.com/NousResearch/hermes-agent/blob/main/hermes_cli/middleware.py)
