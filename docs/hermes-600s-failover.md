# Hermes 600秒stall調査ランブック

## Status

`docs/spec.md` に基づくtimeout分離、request lifecycle log、safe retry、
backend状態機械は実装済み。

この文書は、Hermesで「約600秒出力がない」現象を再観測した場合に、
どの層が待機・切断・再試行を行ったかを特定するための運用手順である。

## 現在のproxy動作

- 定期監視は `GET /v1/models` だけを使用する。
- 通常requestの前にactive inference probeを実行しない。
- connect、response header、first body byte、stream idleを個別にtimeout管理する。
- first body byteを受信するまでdownstream responseをcommitしない。
- response開始後に別backendへretryしない。
- first-body-byte timeoutは既定でretryせず、504を返してbackendをCooldownへ移す。
- retryは再生可能なrequestの接続失敗、またはresponse開始前の502/503/504だけを対象とする。
- 2回目は必ず異なるbackendを選ぶ。
- response bodyのEOFまたはdropまでbackend permitを保持する。

既定値：

```toml
[timeouts]
connect = "5s"
response_headers = "60s"
first_body_byte = "300s"
stream_idle = "300s"

[routing]
max_attempts = 2

[cooldown]
duration = "300s"
consecutive_failure_threshold = 2
```

長いuncached prefillでは正常でもTTFTが長くなるため、first body byte timeoutは
実測に合わせて調整する。

## 重要な制約

DS4がclient disconnect後に必ず推論を停止するとは限らない。

このためfirst-body-byte timeout後にproxyが自動で別backendへ同一requestを送ると、
2台で同じ論理推論を続ける可能性がある。現在の既定実装がfirst-body-byte timeoutを
retryしないのは、この重複実行を避けるためである。

また、各Macでproxyを別processとして動かす場合、`in_flight`とaffinity SQLiteは
process-localである。他proxyの進行中requestは直接観測できない。

## 調査手順

### 1. 実行時設定を保存する

両Macについて次を記録する。

- DS4 Smart Proxy設定
- Hermes provider timeout/retry設定
- DS4起動引数
- Caddy等の中間proxy設定
- LaunchAgentの環境変数

repositoryのexampleと稼働中設定が同じとは仮定しない。

### 2. request IDを全層で相関する

Hermesまたは外側のgatewayから `X-Request-Id` を付ける。proxyは値をupstreamへ
転送し、responseにも返す。

同じIDについて次を並べる。

- Hermes log
- client側DS4 Smart Proxy log
- DS4 backend log
- Caddy access log
- GPU activity

### 3. 最初に発生した境界を特定する

次のどれが最初かを確認する。

- response header timeout
- first body byte timeout
- stream idle timeout
- client cancellation
- upstream stream error
- HTTP 502/503/504
- Hermes側のstale-call detection
- TCP FIN/RST
- 新しいrequest IDによるcaller retry

proxy自身が返すエラーはOpenAI形式JSONの `error.code` で識別できる。

```text
response_header_timeout
first_body_byte_timeout
upstream_connect_failed
upstream_protocol_error
no_backend_available
```

## 観測するlog field

```text
request_id
method
path
backend_id
backend_kind
routing_reason
affinity_source
affinity_key_tag
affinity_hit
in_flight_before
attempt
response_header_ms
first_body_byte_ms
total_ms
status
bytes_in
bytes_out
error_kind
```

session ID、prompt、Authorization、完全affinity hashはログへ出力しない。

## Metrics

特に次を確認する。

```text
ds4_proxy_time_to_first_byte_seconds
ds4_proxy_in_flight
ds4_proxy_backend_health
ds4_proxy_retries_total
ds4_proxy_cooldown_total
ds4_proxy_stream_errors_total
ds4_proxy_affinity_lookup_total
```

`/backends` ではhealth、in-flight、EWMA latency、最終成功・失敗、
cooldown期限を確認できる。

## 判定例

### response headerが遅い

`response_header_ms`がtimeout付近まで増え、`response_header_timeout`になる。
DS4 HTTP frontend、request queue、接続先誤りを確認する。

### headerは返るが最初のtokenが遅い

`response_header_ms`は短いが`first_body_byte_ms`が長い。
長いprefill、worker stall、cache missを疑う。

### token生成途中で止まる

最初のchunkは届くが`stream_idle_timeout`になる。開始後なのでproxyはretryしない。
DS4 logとnetwork resetを確認する。

### 別backendで突然処理が始まる

同じ `request_id` の `attempt=2` ならproxy retryである。別request IDならHermes等の
caller retryである。`ds4_proxy_retries_total`と`routing_reason`を併せて確認する。
