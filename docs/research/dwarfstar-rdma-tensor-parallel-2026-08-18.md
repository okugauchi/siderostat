# DwarfStar RDMA / Tensor Parallel / DSpark 調査報告

- 文書状態: 調査時点のスナップショット
- 調査基準日: 2026-08-18 (JST)
- 対象 upstream: [`antirez/ds4`](https://github.com/antirez/ds4)
- 固定 baseline: [`84cc882352757baf628a1776badf7cc54d584e28`](https://github.com/antirez/ds4/commit/84cc882352757baf628a1776badf7cc54d584e28)
- baseline 日時: 2026-08-09 19:53:31 +02:00
- baseline 件名: `rocm: enable DSpark speculative decoding`
- 対象 Siderostat: 2026-08-18 時点のローカル `main`
- upstream main 追跡日: 2026-08-24 (JST)
- 追跡時点の upstream main: `c1d4597a80e300b803dc642519718f2c999589da`

> 本文は 2026-08-18 時点のスナップショットである。2026-08-24 時点の
> upstream main との差分と、本文を現在の実装に適用する際の訂正は、末尾の
> 「upstream main 追跡更新」に記録する。

## 1. 目的

DwarfStar の最新状態、DeepSeek V4 Flash 0731 以降のモデル対応、分散推論、
RDMA over Thunderbolt、Tensor Parallelism (TP)、DSpark の関係を、Siderostat の
次期機能を検討するための固定日付き資料として残す。

本書では、次の二つを区別する。

- **upstream main で利用できる機能**: 上記固定 baseline のコードと文書で確認できるもの
- **開発中の機能**: issue または未マージ Pull Request に存在し、main へは未反映のもの

未マージ Pull Request の内容は実装方針を示す有力な材料だが、Siderostat の互換性
baseline としては扱わない。

## 2. 結論

1. DwarfStar は引き続き beta かつ高速に変化するソース配布中心のプロジェクトである。
   2026-08-18 時点で GitHub Release は存在せず、Siderostat は commit と実行バイナリの
   digest を固定する現在の方針を維持する必要がある。
2. Flash 系の現行 checkpoint は 0731 である。0731 より新しい Flash checkpoint の
   対応は確認できなかった。一方、モデル系列は GLM 5.2 と DeepSeek V4 PRO へ拡大している。
3. 現行 main の Mac 間 TP は `ds4` CLI のみが公開しており、`ds4-server` と
   `ds4-agent` は TP role を公開していない。これは README にも明記されている。
4. DSpark と TP の組合せ自体は、現行 main の core に明示的な対応がある。leader の
   speculative verify を worker へミラーし、commit または rollback/replay を同期する。
   したがって「DSpark + TP が設計上未検討」という状態ではない。
5. 未成立なのは主に `ds4-server` 側の TP 起動・終了 lifecycle、disk KV restore、
   HTTP session と TP mirrored session の結合、および組合せ QA である。実際に
   `ds4-server` へ TP を配線する二つの未マージ PR が存在する。
6. `ds4` の標準入出力を Siderostat が HTTP へ変換する方式は、機能を限定した PoC なら
   検討できる。しかし現行 CLI 出力は機械向け protocol ではなく、OpenAI 互換 API、SSE、
   tool calls、キャンセル、複数 session を正確に再現する production 経路には適さない。
7. 推奨順は、(A) upstream の `ds4-server` TP 対応を採用、(B) 固定 commit に小さな
   server 配線 patch を保守、(C) 制約付き stdio/PTY bridge で先行検証、である。

## 3. upstream の状態

### 3.1 配布と成熟度

upstream README は DwarfStar を「beta quality」「very fast changing」と位置付けている。
また、汎用 GGUF runner ではなく、限られたモデル、量子化、backend を垂直統合して
最適化する方針を採る。

2026-08-18 時点の確認結果は次のとおりである。

| 項目 | 状態 |
|---|---|
| `main` HEAD | `84cc882352757baf628a1776badf7cc54d584e28` |
| GitHub Release | なし |
| 主対象 | DeepSeek V4 Flash |
| 追加対象 | GLM 5.2、DeepSeek V4 PRO |
| backend | Metal、CUDA、ROCm、診断用 CPU path |
| 主な分散方式 | pipeline parallel、2-node Metal TP、single-host CUDA TP |

GitHub Release がないため、「最新版」という名前だけで運用せず、少なくとも次を一組で
記録しなければならない。

- source commit
- `ds4` / `ds4-server` バイナリ SHA-256
- main GGUF と support GGUF の SHA-256 および size
- backend、量子化、model family、context geometry
- 分散 peer 間で一致を要求する field

これは Siderostat の現行 manifest 方針と一致する。

### 3.2 Siderostat の現行 baseline

2026-08-18 時点の Siderostat は DwarfStar を次の二形態で管理する。

- standalone は `ds4-server` を一つ起動し、OpenAI-compatible HTTP を local upstream とする。
- distributed は `ds4-server --role coordinator|worker --layers ...` を使う pipeline parallel で、
  coordinator だけが HTTP listener を持つ。

DSpark は standalone profile の typed config として実装済みで、main/support model binding、
support digest、confidence、strict、startup log 上の activation を検証する。現行 Siderostat は
DSpark を resident standalone に限定し、distributed argv へは加えない。

process supervisor は child の stdout/stderr を pipe して readiness と metrics を抽出するが、
stdin は pipe していない。また readiness は `ds4-server` の `/v1/models` を前提とする。
したがって `ds4` CLI を採用する TP profile には、既存 command builder の flag 追加だけでなく、
HTTP adapter、stdin/PTY、別 readiness contract が必要になる。

### 3.3 0731 以降のモデル対応

「0731 以降」は、日付だけでなく DeepSeek V4 Flash の 0731 checkpoint として整理する。
現行 baseline では Flash 0731 が current であり、それより新しい Flash checkpoint は
確認できない。ただし、対応する model family と実行形態は次のように拡大している。

| model / support | baseline で確認できる範囲 | 主な制約 |
|---|---|---|
| DeepSeek V4 Flash 0731 | `ds4f-q2`、`ds4f-q2-q4`、`ds4f-q4`、native `ds4f-mxfp4` | 実行形態ごとに対応する量子化 layout が異なる |
| Flash 0731 DSpark support | 約 5.6 GiB の別 support GGUF | `q2` / `q2-q4` / `q4` と組合せる。PRO 非対応。checkpoint 固定 |
| GLM 5.2 | 指定された Unsloth Q4、および antirez IQ2_XXS / Q2 / Q4 layout | 任意の GLM GGUF は非対応。Mac 間 TP は ownership-aware IQ2_XXS または Q2_K routed layout が必要 |
| DeepSeek V4 PRO | 512 GiB 級の q2、二台向け Q4 layer split、128 GiB Mac の experimental SSD streaming | DSpark 非対応。高い memory requirement |

DSpark support GGUF は単独モデルではない。Flash 0731 main model と一緒に、概ね次の形で
指定する。

```sh
./ds4 \
  -m /path/to/DeepSeek-V4-Flash-0731.gguf \
  --mtp /path/to/DeepSeek-V4-Flash-DSpark-support-0731.gguf \
  --dspark \
  --temp 0
```

`--mtp` は support GGUF の指定、`--dspark` は DSpark runtime の選択である。DSpark は
greedy decoding の future token を draft するもので、prefill は高速化しない。sampling、
`--quality`、`--dspark-strict` では通常の target-only decode になる。Siderostat は
「DSpark enabled」だけでなく、main checkpoint と support checkpoint の binding を
fail-closed で検証し続ける必要がある。

## 4. 分散推論方式の整理

### 4.1 比較

| 方式 | model の分割 | token の進み方 | 主な通信 | 現行 HTTP | 主目的 |
|---|---|---|---|---|---|
| pipeline parallel | transformer layer 範囲 | 各 token が node を順に通る | TCP activation transfer | `ds4-server` で利用可能 | 単一 node に入らない model、prefill pipeline |
| Mac 間 TP | routed expert を 50/50、dense 等は複製 | 二台が同じ token を lockstep 実行 | RDMA または専用 TCP gate | main では `ds4` CLI のみ | per-token latency 低減、resident capacity |
| single-host CUDA TP | GPU 間で分割 | 同一 host 内の並列実行 | CUDA/NCCL 系 | `ds4-server` で利用可能な path あり | throughput / capacity |
| 開発中 network CUDA/ROCm TP | node 間で分割 | lockstep | NCCL または verbs | PR 内で server 配線あり | DGX Spark / Strix Halo cluster |

pipeline parallel と TP は、同じ「二台推論」でも運用上別 profile として扱う必要がある。
TP は `--layers` を使わず、必ず 50/50 の coordinator/worker pair となる。Siderostat の
現行 `distributed-mxfp4` profile を flag だけ差し替えて TP に流用してはならない。

### 4.2 Mac 間 TP の main 実装

現行 README と core で確認できる条件は次のとおりである。

- Apple silicon Mac 二台
- 50/50 の coordinator と worker
- 同一 source tree、commit、GGUF
- resident weight が前提で、`--ssd-streaming` は不可
- `--transport rdma` または `--transport tcp`
- RDMA 利用時は Thunderbolt member interface 自身の IPv4/GID を使い、bridge address を
  coordinator address として使わない
- worker を先に起動し、coordinator の load 完了を待つ

代表的な起動形は次である。

```sh
# worker
./ds4 -m "$MODEL" --tensor-parallel --role worker \
  --coordinator 10.99.0.2 9911 --transport rdma

# coordinator
./ds4 -m "$MODEL" --tensor-parallel --role coordinator \
  --listen 10.99.0.2 9911 --transport rdma -c 8192 \
  -p "Tell me something about the sea."
```

Apple の [TN3205](https://developer.apple.com/documentation/technotes/tn3205-low-latency-communication-with-rdma-over-thunderbolt)
によれば、RDMA over Thunderbolt は macOS 26.2 以降、Apple silicon、Thunderbolt 5 の
組合せで利用できる。Recovery で `rdma_ctl enable` を実行する必要があり、通常の IP ping
成功だけでは RDMA の readiness を証明できない。`ibv_devices`、`ibv_devinfo`、
`PORT_ACTIVE`、対象 IPv4 に対応する GID まで確認する必要がある。

### 4.3 実測情報の扱い

open issue [#651](https://github.com/antirez/ds4/issues/651) には、二台の M4 Max と
Thunderbolt 5 上の TCP で、Flash 0731 の TP CLI が decode 22.7 t/s、`ds4-server` の
pipeline が約 19–20 t/s だったという field report がある。

これは TP の可能性を示すが、単一利用者の測定であり、Siderostat の acceptance value
ではない。model、context、prompt、warm state、transport、電力状態を固定した再測定が必要である。

## 5. DSpark と TP

### 5.1 「検討されている形跡」の有無

形跡はあるだけでなく、現行 main の core に具体的な実装がある。

`ds4_tp_validate_engine_options()` は、leader 上の DSpark/MTP speculative drafting を
許可すること、verify block を `DS4_TP_FRAME_VERIFY` で worker にミラーすることを
コメントで明示している。TP protocol には、少なくとも次の session command がある。

- session create / destroy
- sync
- single-token eval
- rewind / invalidate
- eval batch / mixed batch
- speculative verify
- verify commit

worker は `ds4_session_tp_spec_cycle()` で leader と同じ verify block の片側を実行し、
KV、compressor、indexer の副作用を同期する。その後 leader からの commit frame に従い、
full accept なら状態を保持し、partial/reject なら rollback して accepted prefix を
lockstep replay する。

したがって、現行の技術的整理は次のとおりである。

- DSpark proposal を作るのは leader
- target verification は TP の両 rank で行う
- commit/rollback decision は worker に同期される
- legacy MTP は TP 時に token-by-token fallback となる箇所がある

### 5.2 まだ保証されていないもの

core に実装が存在することと、配布可能な server profile として保証されることは同義ではない。
少なくとも次は未完了または acceptance 未実施である。

- upstream main の `ds4-server` から Mac 間 TP role を起動すること
- Flash 0731 + DSpark + Mac 間 TP + RDMA の固定 matrix QA
- client disconnect、cancel、tool continuation と speculative TP の組合せ
- long context、multi-turn、worker reconnect、rank desync の endurance
- disk KV restore と mirrored session の整合
- native `--batched-session` との組合せ

現行 `ds4-server` は `--mtp`、`--dspark`、`--dspark-confidence`、
`--dspark-strict` を受理し、single-session、greedy の経路では speculative argmax を
呼ぶ。一方、`--batched-session` が有効な場合は speculative decoding を無効化する。
最初の DSpark + TP server PoC は、native batching を使わず `max_in_flight = 1` から
開始すべきである。

## 6. `ds4` と `ds4-server` の差分

### 6.1 共通部分

固定 baseline の Makefile では、`ds4` と `ds4-server` は `ds4.o`、`ds4_tp.o`、
backend object などの core を共有する。単純な規模の目安として、front-end source は
次の行数である。

| file | 行数 |
|---|---:|
| `ds4_cli.c` | 2,205 |
| `ds4_server.c` | 18,450 |

TP graph、transport、mirrored session、DSpark verification は core 側にある。そのため
server 対応の本質は TP を再実装することではなく、server の option、engine bind、worker
loop、session lifecycle、shutdown、KV policy へ正しく配線することである。

### 6.2 server 固有部分

`ds4-server` は CLI 出力を HTTP に包んだだけではない。少なくとも次を server 自身が持つ。

- OpenAI 互換 chat/completions と completions
- Responses / Anthropic 互換を含む追加 endpoint
- SSE streaming
- request JSON validation と sampling option
- tool call / reasoning / continuation の状態機械
- HTTP client disconnect と cancel
- session slot、native batching、mixed prefill/decode
- disk KV cache、resume、session rewind
- usage、logprobs、model metadata

したがって、CLI の stdout を parse しても `ds4-server` と「同等」の interface にはならない。
同等性を要求する場合は server front-end を TP core に接続する方が小さい保守範囲になる。

### 6.3 upstream の開発中 server 対応

2026-08-18 時点で次の open PR が確認できる。

| PR | 対象 | server 配線 | 注意点 |
|---|---|---|---|
| [#813 Two-machine tensor parallelism for the ROCm backend](https://github.com/antirez/ds4/pull/813) | ROCm verbs、既存 Metal TP の一般化 | TP CLI parse/validate、leader bind、mirrored worker loop を `ds4_server.c` に追加 | disk KV restore は rank desync 回避のため TP leader で無効化。未マージ |
| [#754 CUDA: add DGX Spark network expert/tensor parallelism](https://github.com/antirez/ds4/pull/754) | 2/4 rank CUDA network TP/EP | `ds4-server`、agent、eval 等へ lifecycle を配線 | NCCL path。外部 DSpark/MTP との組合せには制約。未マージ |

これらは「distributed inference の server 対応が検討されていない」という見方を否定する。
特に #813 は、現行 main で CLI 専用だった TP を server へ出す変更を明示している。
ただし、main HEAD より新しい未マージ code なので、現時点の正式依存先にはできない。

## 7. Siderostat による stdio / PTY HTTP bridge の評価

### 7.1 判定

**限定 PoC としては実現余地がある。production の標準構成としては非推奨である。**

現行 Siderostat の `ManagedChild` は stdout/stderr を pipe するが stdin は pipe していない。
stdin の追加自体は小さい変更である。しかし難所は pipe の有無ではなく、CLI の対話表示を
安定した request/response protocol として扱えるかどうかにある。

```text
HTTP client
    │ OpenAI-compatible subset
    ▼
Siderostat HTTP adapter
    │ serialize: max 1 request
    │ prompt rendering / output framing / cancel
    ▼ stdin + stdout/stderr または PTY
ds4 TP coordinator CLI  ───── RDMA/TCP ─────  ds4 TP worker CLI
```

### 7.2 現行 CLI をそのまま bridge する場合の問題

- interactive mode は `linenoise` と人間向け prompt/output を使う
- stdout に token text、prompt、色、終了表示が混在し得る
- stderr の診断行と生成失敗を machine-readable に対応付ける request ID がない
- multi-line user content と `/` command の境界が protocol として定義されていない
- OpenAI messages から DwarfStar chat template への完全な変換が必要
- finish reason、usage、logprobs、reasoning、tool call を stdout だけから再構成できない
- client disconnect 時の generation cancel と次 request の stream 境界回復が難しい
- 一つの REPL session を複数 HTTP request で安全に multiplex できない
- upstream の表示変更で parser が破損する
- pipe と TTY で `isatty()` による表示差があるため、純粋な pipe より PTY の方が動作を
  再現しやすいが、presentation protocol 依存という根本問題は残る

### 7.3 PoC として許容する最小 contract

bridge を先行実装する場合は、`ds4-server` 同等を名乗らず、次に限定する。

| 項目 | PoC contract |
|---|---|
| endpoint | `GET /v1/models`、`POST /v1/chat/completions` の最小 subset |
| concurrency | `max_in_flight = 1` |
| sampling | greedy (`temperature = 0`) のみ |
| streaming | text delta のみ。usage/tool delta は対象外 |
| session | 単一 persistent TP session |
| tool calls | 非対応 |
| logprobs / Responses / Anthropic | 非対応 |
| native batching | 無効 |
| KV disk restore | TP 整合が保証されるまで無効 |
| transport fallback | RDMA required または明示的 TCP。silent fallback は禁止 |

さらに、可能なら人間向け REPL を scrape せず、DwarfStar に小さな `--stdio-json` または
length-prefixed request/response mode を追加する。この場合も HTTP semantic の実装主体は
Siderostat になるが、token と lifecycle の framing は安定させられる。

### 7.4 推奨する選択順

1. **upstream server 対応を採用する**
   - main へ merge された TP server lifecycle と QA を利用する。
   - Siderostat は process orchestration、preflight、failover、HTTP routing に集中する。
2. **固定 baseline に最小 server patch を適用する**
   - #813/#754 の配線を参照し、必要な backend だけを backport する。
   - DwarfStar core と server semantic をそのまま使える。
3. **stdio/PTY bridge を実験 profile として実装する**
   - upstream merge 前の性能・運用検証に限定する。
   - public default にせず、feature flag と明示的な API 制約を付ける。

## 8. Siderostat への設計上の影響

### 8.1 新しい profile

現行の `solo-standalone`、`paired-standalone`、`distributed-mxfp4` に加え、TP は別 profile
として設計する。

```text
tensor-parallel-rdma
tensor-parallel-tcp   # 診断・fallback を明示する場合だけ
```

profile は次を保持する。

- model family / checkpoint / quant layout
- main GGUF path、size、SHA-256
- DSpark support path、size、SHA-256、confidence、strict
- DwarfStar source commit と両 node の binary digest
- context size、prefill geometry、warm/resident policy
- rank、coordinator address、member interface、port
- transport policy、RDMA device、GID index
- disk KV policy
- server frontend 種別 (`ds4-server-tp` / experimental `stdio-bridge`)

### 8.2 preflight

Siderostat は model load 前に両 node で次を検証する。

- macOS 26.2 以上、Apple silicon、Thunderbolt 5
- `rdma_ctl` 有効化状態
- verbs device と `PORT_ACTIVE`
- member interface の IPv4 と対応 GID
- peer の direct member address 到達性
- 同一 commit / approved binary digest / GGUF digest
- TP 対応 quant layout
- context size と session geometry の一致
- resident memory headroom と `iogpu.wired_limit_mb`

RDMA 指定時に条件を満たさなければ fail-closed とする。自動で TCP に落とす場合は、設定で
`transport = "auto"` を明示し、実際に選ばれた transport を API、metrics、monitor に表示する。

### 8.3 lifecycle

TP startup は pipeline と別の state machine を持つ。

1. 両 node の standalone admission を停止する。
2. worker を先に起動する。
3. worker が coordinator 接続待ちになったことを確認する。
4. coordinator を起動し model を bind する。
5. rank handshake、model/session geometry、transport を確認する。
6. HTTP ready または bridge ready を確認する。
7. generation を公開する。

一方の rank を失った場合、残った rank だけで generation を継続してはならない。request を
drain/cancel し、両 TP child を identity-verified stop してから standalone へ戻す。

### 8.4 observability

既存 metrics に加え、少なくとも次を公開する。

- `siderostat_tp_transport{transport="rdma|tcp"}`
- `siderostat_tp_rank_ready{rank="leader|worker"}`
- `siderostat_tp_session_mirrored`
- `siderostat_tp_gate_errors_total`
- `siderostat_tp_desync_total`
- `siderostat_tp_rdma_device_info`
- `siderostat_tp_dspark_verify_cycles_total`
- `siderostat_tp_dspark_accepted_tokens_total`
- `siderostat_tp_fallback_reason`

## 9. 検証計画

### Phase 0: upstream 追跡

- #651、#813、#754 と main の TP/server 変更を追跡する。
- merge commit を新しい DwarfStar candidate baseline として別途固定する。
- server shutdown、worker stop、disk KV、DSpark/batching policy を diff review する。

### Phase 1: 手動 TP baseline

- `ds4` CLI、TCP で TP correctness と throughput を測定する。
- 同じ matrix を RDMA で測定する。
- hidden-state hash check、長い prefill、multi-turn、peer loss を含める。

### Phase 2: HTTP path

- merge 済み `ds4-server` TP を優先して試す。
- 利用できなければ experimental stdio/PTY bridge を最小 contract で試す。
- 同一 prompt の output、usage、cancel、SSE framing を比較する。

### Phase 3: DSpark combination

- Flash 0731 の main/support digest を固定する。
- target-only、DSpark single-node、TP target-only、TP + DSpark を比較する。
- acceptance rate、decode t/s、first-token latency、desync、output identity policy を記録する。

### release gate

次を満たすまでは、TP を Siderostat の既定 distributed mode にしない。

- 24 時間以上の peer-loss/reconnect endurance
- cancellation と client disconnect 後の次 request の正常性
- rank desync を検出して fail-closed できること
- model、binary、geometry mismatch の load 前拒否
- RDMA unavailable 時の policy 通りの挙動
- monitor と admin API から実 transport と rank readiness を確認できること
- pipeline/standalone への rollback が既存 acceptance を退行させないこと

## 10. 参照資料

### DwarfStar

- [DwarfStar repository](https://github.com/antirez/ds4)
- [固定 baseline commit](https://github.com/antirez/ds4/commit/84cc882352757baf628a1776badf7cc54d584e28)
- [固定 baseline README](https://github.com/antirez/ds4/blob/84cc882352757baf628a1776badf7cc54d584e28/README.md)
- [固定 baseline `ds4_tp.c`](https://github.com/antirez/ds4/blob/84cc882352757baf628a1776badf7cc54d584e28/ds4_tp.c)
- [固定 baseline `ds4.c`](https://github.com/antirez/ds4/blob/84cc882352757baf628a1776badf7cc54d584e28/ds4.c)
- [固定 baseline `ds4_server.c`](https://github.com/antirez/ds4/blob/84cc882352757baf628a1776badf7cc54d584e28/ds4_server.c)
- [Issue #651: `ds4-server` TP roles](https://github.com/antirez/ds4/issues/651)
- [PR #813: ROCm two-machine TP / server wiring](https://github.com/antirez/ds4/pull/813)
- [PR #754: CUDA network TP/EP](https://github.com/antirez/ds4/pull/754)
- [GitHub Releases（調査時点で release なし）](https://github.com/antirez/ds4/releases)

### Apple

- [TN3205: Low-latency communication with RDMA over Thunderbolt](https://developer.apple.com/documentation/technotes/tn3205-low-latency-communication-with-rdma-over-thunderbolt)

## 11. upstream main 追跡更新（2026-08-24 JST）

2026-08-18 の固定 baseline `84cc882` から、upstream `main` は
`c1d4597a80e300b803dc642519718f2c999589da` まで進んでいる。今回確認した範囲では、
ROCm/MXFP4、DSpark のサンプリング、DeepSeek V4 PRO 0813 の品質検証とモデル取得が
更新されている。一方、Mac の `ds4-server` で tensor parallel を起動する経路、
Mac の layer-parallel を RDMA 化する経路、distributed DSpark を
`ds4-server` の運用経路に統合する変更は、この main には入っていない。

### 11.1 DSpark / MTP の現在の扱い

- 通常の `--dspark` は、非ゼロ温度でも通常の target sampling と DFlash の
  temperature-zero draft を組み合わせる opportunistic sampling になった。draft が
  target の greedy continuation と一致した部分はそのまま採用するため、通常の温度
  サンプリングより決定的になる場合がある。
- target の分布を維持したい場合は `--mtp-exact-sampling` を使用する。このモードは
  greedy draft を target probability で受理し、棄却時は残余の target distribution
  からサンプリングする。デフォルトの confidence threshold は `0.8` で、
  `--temp 0` を加えると fully greedy になる。
- 同じ DSpark オプションは `ds4`、`ds4-agent`、および非 batched の
  `ds4-server` で利用できる。`--batched-session` による native session batching
  中は speculative decoding を行わず、通常の target decoding を使用する。
- DSpark のサポート checkpoint は引き続き Flash 0731 専用であり、PRO は未対応である。
  したがって、この更新は「distributed DSpark が本番 server に入った」ことを意味しない。

### 11.2 モデルとバックエンドの更新

- Flash の基準 checkpoint は引き続き 0731。新しい Flash checkpoint への更新は確認できない。
- `pro-q2-imatrix` の取得対象が DeepSeek V4 PRO 0813 の q2 imatrix GGUF に更新され、
  `gguf-tools/quality-testing/data/pro-0813/` に 100 ケースの品質 oracle が追加された。
  これは Flash 0731 の更新ではなく、PRO の別 checkpoint / model family である。
- ROCm では routed-expert の native MXFP4 decode/prefill kernel、occupancy variant、
  検証用テストと Strix Halo の検証手順が main に入った。README の Strix Halo 分割例は
  `--layers 0:21` と `--layers 22:output` である。ただし、これは ROCm/Strix Halo の
  実装であり、Apple Metal の Mac-to-Mac TP や layer-parallel RDMA を追加するものではない。

### 11.3 Siderostat の採用判断への反映

2026-08-24 時点で、Siderostat の実機構成については次の整理を維持する。

| 管理対象 | upstream main の確認結果 | Siderostat での扱い |
| --- | --- | --- |
| Apple 2台の layer-parallel | TCP の activation transfer | 現行 v0.3 の対象。RDMA は将来候補 |
| Apple 2台の tensor parallel | `ds4` CLI の worker/coordinator のみ | `ds4-server` の管理対象としては未採用 |
| Apple 2台の layer-parallel RDMA | current main に実装なし | 将来タスクのまま |
| distributed DSpark server | current main に統合なし | 将来タスクのまま |
| ROCm MXFP4 | Strix Halo 向けに main へ統合 | Apple 構成の transport/topology 変更とは分離 |

従って、本文中の PR・PoC・実機測定を、現行 upstream main の production capability と
読み替えてはいけない。特に PR #813、#754、#835、#715 は、今回の baseline 比較における
「関連する将来候補」であり、main にマージ済みの Mac server TP / RDMA 機能を示す証跡ではない。

### 11.4 追跡に使用した upstream の証跡

- [upstream main（追跡時点）](https://github.com/antirez/ds4/commit/c1d4597a80e300b803dc642519718f2c999589da)
- [ROCm MXFP4 kernel 実装](https://github.com/antirez/ds4/commit/39a8f18)
- [DSpark exact sampling](https://github.com/antirez/ds4/commit/769a8ba)
- [DeepSeek V4 PRO 0813 oracle](https://github.com/antirez/ds4/commit/3c63c06)
- [PRO 0813 model download target](https://github.com/antirez/ds4/commit/c35cf38)

## 12. upstream main 追跡更新（2026-08-26 JST）

2026-08-26 に upstream の公開 `main` とローカル参照 `origin/main` を再確認した。
固定 baseline `84cc882352757baf628a1776badf7cc54d584e28` に対し、確認対象の main は
`c1d4597a80e300b803dc642519718f2c999589da`（2026-08-23、`qa: update DGX Spark host addresses`）で、
19 commit 進んでいる。main の履歴と README の記述をソースとして照合した結果、前節の
2026-08-24 時点の判断に、次の補正を加える。

### 12.1 main に入った更新と TP への影響

- DeepSeek V4 PRO 0813 の q2 imatrix 取得対象と 100 ケースの品質 oracle が追加された。
  これは Flash 0731 の更新ではなく、別 model family／checkpoint の追加である。
- DSpark は非ゼロ温度の opportunistic sampling と `--mtp-exact-sampling` を備え、`ds4`、
  `ds4-agent`、非 batched の `ds4-server` で利用できる。`--batched-session` では speculative
  decoding を使用しない。この変更は single-node／server の decoding policy であり、distributed
  pipeline の DSpark lifecycle を main に追加したものではない。
- main README には DeepSeek V4 PRO Q4 の二台 layer split と、M5 Max 二台の pipeline 測定値が
 追加されている。これは plain TCP の layer-parallel であり、Mac 間 TP または RDMA ではない。
- Mac 間 tensor parallel は `ds4 --tensor-parallel --role coordinator|worker` として README と
  `ds4_tp.c` に存在する。一方 `ds4_server.c` の main 側 option には Mac TP の
  `--tensor-parallel` role parser はなく、確認できる `--cuda-tensor-parallel` は単一 CUDA host の
  別経路である。main README も Mac TP roles は `ds4` CLI に限られ、`ds4-server`／`ds4-agent` には
  公開されていないと明記している。

### 12.2 関連 PR の状態（main へ未採用）

2026-08-26 の公開状態は次のとおりである。Open／Closed は PR の状態であり、採用済み capability
とは解釈しない。

| PR | 内容 | 公開状態 | T-01 での扱い |
| --- | --- | --- | --- |
| [#813](https://github.com/antirez/ds4/pull/813) | ROCm two-machine TP と `ds4-server` 配線 | Open | main 依存にしない。ROCm 専用候補として追跡 |
| [#754](https://github.com/antirez/ds4/pull/754) | CUDA network EP/TP と server/agent 配線 | Open | CUDA 専用候補として追跡 |
| [#835](https://github.com/antirez/ds4/pull/835) | layer-parallel の distributed MTP/DSpark | Open | merge・QA 完了までは採用しない |
| [#715](https://github.com/antirez/ds4/pull/715) | Apple layer-parallel RDMA | Closed | main への採用証跡なし。候補から再利用しない |

したがって、現時点で「関連実装が存在する」ことと「Siderostat が管理対象として採用できる」
ことを分離する必要がある。特に PR の説明・測定値だけでは server lifecycle、互換性、rollback、
再接続時の安全性を証明できない。

### 12.3 T-01 の延期判定と v0.4+ backlog

T-01 の判定は **v0.4+ へ延期** とする。v0.3.0 の現行対象は引き続き Apple 2台の
`Distributed (layer-parallel)`／TCP とし、TP、RDMA layer-parallel、distributed DSpark を
config、manifest、state machine、metrics の production profile として追加しない。

v0.4+ で再開する候補は、次の順序に固定する。

1. **upstream server path**: upstream main に対象 backend の server role／lifecycle が入り、
   `ds4-server` の HTTP contract、session、cancel、shutdown、KV policy が文書化・テスト済みで
   あることを確認する。未採用の場合は、固定 commit に限定した最小 backport を別計画として承認する。
2. **HTTP contract gate**: `healthz`／model metadata、streaming、client disconnect、request cancel、
   session persistence、tool/continuation の扱いを、single-node server と同じ意味で定義する。
   CLI stdout/PTY scrape を production dependency にしない。stdio/PTY bridge は制約付き PoC に限定する。
3. **24時間 endurance gate**: RDMA と明示的 TCP の各 transport で、request、long prefill、cancel、
   peer loss/reconnect、worker replacement、multi-turn、session/KV restore を固定 model/source digest
   で実施し、deadlock、rank desync、request duplication、orphan がないことを確認する。
4. **fail-closed gate**: source commit、binary digest、model／quantization、topology、world size、
   context geometry、transport capability の不一致を起動前に拒否する。RDMA unavailable 時の TCP
   fallback は暗黙に行わず、設定で明示された場合だけ許可する。trusted network 前提と未認証 protocol
   を Monitor に「ready」と表示しない。
5. **rollback gate**: TP／RDMA child、peer control、local standalone、layer-parallel へ戻る遷移を
   lifecycle owner 経由で実装し、途中失敗時の admission、in-flight、state、generation、orphan を
   既存 v0.3 acceptance と同じ基準で確認する。

T-01 の事後条件を満たすため、上記判断を Siderostat の v0.3 実装へ反映せず、次版の entry
criteria としてのみ記録する。これにより v0.3 の release candidate に未成熟な TP／RDMA の
placeholder や silent fallback が混入することを防ぐ。

### 12.4 追跡証跡

- [upstream main commit `c1d4597`](https://github.com/antirez/ds4/commit/c1d4597a80e300b803dc642519718f2c999589da)
- [upstream main commit history](https://github.com/antirez/ds4/commits/main)
- [upstream README の Tensor Parallelism over RDMA](https://github.com/antirez/ds4/blob/c1d4597a80e300b803dc642519718f2c999589da/README.md#tensor-parallelism-over-rdma)
- [DSpark exact sampling](https://github.com/antirez/ds4/commit/769a8ba)
- [DeepSeek V4 PRO 0813 oracle](https://github.com/antirez/ds4/commit/3c63c06)
- [PRO 0813 model download target](https://github.com/antirez/ds4/commit/c35cf38)
