# antirez/ds4 調査・ディスカッション概要レポート

**対象期間:** 2026-08-21 22:07頃 〜 2026-08-22 09:52 JST  
**対象:** `antirez/ds4` の最新開発状況と、ローカルLLM運用・分散推論・ speculative decoding 周辺の検討  
**関連環境:** Apple Silicon 2台構成、Thunderbolt接続、Hermes Agent、Siderostat

**upstream main 追跡更新:** 2026-08-24 (JST)。本文は調査期間の記録であり、
その後の `antirez/ds4` `main` との差分と訂正は末尾の「upstream main 追跡更新」に記録する。

---

## 1. エグゼクティブサマリー

今回の調査では、`antirez/ds4` の最新開発状況を中心に、以下の論点を確認・整理した。

- DeepSeek V4 Flash 0731 / MXFP4 の reasoning mode
- Disk KV cache の運用ポリシー
- 1M context と複数セッション並列実行の意味
- DeepSeek V4 Flash の Prefill 負荷分布と distributed layer split
- Tensor Parallelism (TP) と RDMA over Thunderbolt の進展
- distributed pipeline + DSpark の開発状況
- DSpark組み込みモデルと DFlash2 の動向
- DeepSeek V4 Flash Vision Exp の ds4 対応状況
- Apple RDMA over Thunderbolt の正式サポートに向けた状況
- `ds4-smart-proxy` から `siderostat` への名称変更と今後の位置づけ

最も重要な結論は、**ds4 の分散推論・speculative decoding・Apple Silicon最適化が急速に成熟しており、現在の TCP over Thunderbolt の layer-parallel 構成は短期的には妥当だが、数か月以内に RDMA / TP / distributed DSpark を再評価する価値が高い**という点である。

---

## 2. ds4 最新開発状況

調査時点で `antirez/ds4` の `main` は 2026-08-09 のコミット `84cc882` 付近が基準となっており、その後の大きな機能は主にPR上で進行していた。

main付近では以下が進展していた。

- M5 Max 向け Metal decode 最適化
- pre-M5 Apple Silicon への最適化展開
- MXFP4 decode / prefill 高速化
- long-context correctness 修正
- DSpark verifier pipeline 改善
- ROCm向け DSpark対応
- server/tool-call周辺の安定化

開発が止まっているのではなく、**mainへの大規模merge前に複数の機能PRが並行して成熟している状態**と判断した。

---

## 3. Tensor Parallelism と RDMA over Thunderbolt

### 3.1 TP の現状

ds4では、2台Macを用いた tensor parallelism がすでに存在する。

基本構造は次の通り。

```text
Mac A
  dense / attention
  routed experts half A

Mac B
  dense / attention
  routed experts half B

      ⇅
  gate partial exchange
```

layer-parallel distributed と異なり、両ノードが同一tokenの処理を共同で行う。

### 3.2 TP高速化 PR

特に重要なのが PR #743。

Metal TP に polled release fence を導入し、同期コストを低減する内容で、M3 Ultra × 2 の実測では以下の報告があった。

```text
baseline TP              16.56 t/s
fast sync                20.44 t/s
fast sync + keepalive off
                         33.12 t/s
```

まだ opt-in / 未merge だが、**TPのdecode性能が大幅に改善する可能性**を示している。

### 3.3 ds4-server でのTP

現行mainでは TP は主に `ds4` CLI 向けで、`ds4-server` からの利用は未成熟。

一方、PR #813 では ROCm向け two-machine TP とともに `ds4_server.c` のTP wiringも進められている。

今後、HTTP API経由でTPを使えるようになれば、Hermes / Siderostat との統合価値が高い。

---

## 4. layer-parallel distributed + Apple RDMA

現在の運用構成は layer-parallel distributed で、

```text
M4 Max Mac Studio
  coordinator
  layers 0:19

M5 Max MacBook Pro
  worker
  layers 20:output
```

という構成。

PR #715 では、これに Apple RDMA over Thunderbolt を導入する実装が進んでいる。

追加予定の主な指定は以下。

```bash
--dist-transport rdma
--dist-rdma-adj-devices ...
```

2ノードでの実測では、Prefillはほぼ変わらない一方、decodeは概ね10〜15%程度改善していた。

例:

```text
TCP   22.88 t/s
RDMA  26.04 t/s
```

これは、

```text
Prefill:
  compute-bound

Decode:
  per-token communication latency-sensitive
```

という性質と整合する。

3ノード以上ではTCPのhop遅延が効きやすく、RDMAの価値がさらに高くなる。

---

## 5. Apple RDMA over Thunderbolt のOS側動向

従来は Recovery 環境から `rdma_ctl enable` を実行する必要があるという扱いだった。

一方、WWDC26では System Settings 上から

```text
Enable RDMA over Thunderbolt
```

を有効化するGUI手順が紹介された。

このため、現時点では以下のように整理した。

```text
macOS 26系:
  Recovery + rdma_ctl enable が実運用手順

次期macOS:
  GUIでの正式設定が一般化する可能性が高い
```

ユーザー側では、WWDC26でのデモというタイミングから、**正式な一般提供は次期macOS（27系と推測）に合わせる可能性が高い**と判断。

そのため、現時点で無理にRecovery経由でRDMAを有効化せず、次期OSとds4側のmerge状況が揃うまで待つ方針が合理的という結論になった。

---

## 6. distributed pipeline + DSpark

直近の重要PRが #835。

これは、

```text
layer-parallel distributed
+
MTP / DSpark speculative decoding
```

を組み合わせるもの。

設計上、後半layerとoutput head、DSpark capture layerを持つworker側がdraftを生成し、coordinatorがtarget-model verificationを行う。

主な特徴:

- greedy decodingのみ
- workerがdraft生成
- coordinatorがbatched verify
- partial accept時のprefix commit
- full accept時のspan chaining
- fallback時は通常decodeへ戻る

現状でも support GGUF を両側で指定する。

```bash
--mtp DeepSeek-V4-Flash-DSpark-support-0731.gguf
--dspark
```

したがって、**distributed pipeline + DSpark はかなり実装が進んでおり、mainへ入る可能性が比較的近い機能**と見てよい。

---

## 7. DSpark の現在地

### 7.1 DSpark組み込みモデル

DeepSeek公式には `DeepSeek-V4-Flash-DSpark` があり、モデル側にDSpark構造を含む。

一方、ds4 mainの実装は依然として、

```text
main GGUF
+
DSpark support GGUF
```

のsidecar方式。

Issue #468 では、公式DSpark variantをds4に統合したいという話題があり、**「組み込みDSparkモデルを直接扱う」方向性自体は認識されている**。

ただし、現時点ではmainにその統合方式は入っていない。

### 7.2 DSpark最適化

直近PRではDSparkそのものの性能改善が活発。

例:

- #833: cold miss後のskip戦略見直し
- #778: first-token forwardをverify batchへfold
- #772 / #776: CUDA向けconfidence / scheduler最適化
- #670: ROCmでDSparkを実動作させる
- #835: distributed pipeline対応
- #846: Metal M1系でのverifierコスト解析

特にMetalでは、DSparkのacceptance率が高くてもverify batch自体が重く、plain decodeより遅くなるケースも報告されている。

したがって現状は、

```text
「DSparkを動かす」
  ↓
「backend / device / workloadごとに本当に得か最適化する」
```

という段階に移っている。

---

## 8. DFlash2

DFlash2はDSparkとは別のspeculative decoding系統で、block diffusion方式。

現時点では DeepSeek V4 Flash の主要経路ではなく、Qwen3.8系を中心に実装されている。

ds4では PR #837 で、

```text
Qwen3.8 27B
+
DFlash2 speculative decoding
```

の大規模実装が進んでいる。

主な追加要素:

```text
--dflash
--dflash2
--draft-model
--spec-type draft-dflash
--dflash-n-max 7
```

---

## 9. DeepSeek V4 Flash Vision Exp

2026-08-21付近で `deepseek-v4-flash-vision-exp` のAPI提供が話題になった。

ただし調査時点では、

- DeepSeek API側では Vision Exp の存在が確認される
- Hugging Face上でopen weightsは確認できない
- ds4側に専用branch / PR / commitは確認できない

という状態。

したがってds4側では、まだ具体的な対応段階に入っていないと判断した。

Vision対応には少なくとも、

```text
vision encoder
projector
image preprocessing
GGUF metadata / tensors
multimodal API parsing
Metal / CUDA / ROCm runtime
```

が必要であり、weights公開後に本格的な対応が始まる可能性が高い。

---

## 10. DeepSeek V4 Flash の Prefill負荷と layer split

DeepSeek V4 Flash は43層構成。

主要な圧縮attention構成は概ね、

```text
layer 0-1:
  sliding-window系

layer 2-42:
  ratio 4 / ratio 128 の圧縮層が交互
```

という構造。

ratio 4側は indexer / sparse attention を伴い、長文脈Prefillでは比較的重い。

重要なのは、**前半layerだから重い、後半layerだから軽い、という単純な構造ではない**という点。

現在の分割:

```text
M4 Max:
  0:19
  20 layers

M5 Max:
  20:output
  23 layers
```

は、結果的にM5側により多くの層とratio-4層を寄せているため、かなり妥当。

改善候補としては、前後を逆転するより、

```text
0:17 / 18:output
0:15 / 16:output
```

のように、M5 Max側へさらに数層寄せるA/Bテストの方が有望。

特に500K級のPrefillで、

```text
coordinator stage time
worker stage time
idle time
activation transfer time
```

を観測して最適splitを決めるのがよい。

---

## 11. 1M context と複数セッション

`--ctx 1000000` は、

```text
全クライアント合計で1M
```

という意味ではなく、

```text
各sessionが到達可能な最大context長
```

に近い。

したがって、

```text
session A = 500K
session B = 500K
```

を同時に扱うことは設計上可能。

ただし、メモリ上は各sessionに独立したKV状態が必要。

```text
model weights
+ KV(A)
+ KV(B)
+ scratch / activation
+ server batching
```

の総量で制約される。

最近のds4-serverではbatched serving / prefill schedulingが改善されており、複数sessionを並行しても以前ほど「片方が完全に止まる」感覚が出にくい。

この観測は、実際にユーザーが感じている

> 並列実行しても待機時間や目詰まりが減った

という現象と整合する。

---

## 12. Disk KV cache の運用

Disk KV cacheについては、単純なLRUではなく、prefix再利用価値・checkpoint理由・hit実績を用いる方向に進化している。

現状の課題は、**miss時に長時間探索した後で結局cache missになることがある**点。

Hermesのような長時間agent sessionでは、

```text
long session
tool output accumulation
subtask
compaction
prompt rewrite
```

が発生し、古いprefix checkpointが再利用できないケースが増える。

推奨方針:

- Disk KV容量はむやみに増やさない
- model / chat template / DS4大更新時はcache世代を切る
- 古い未使用checkpointを整理
- compaction後は旧checkpointを低優先度扱い
- session affinityを維持
- lookup time budget導入が理想

将来的には、

```text
--kv-disk-lookup-budget-ms 1000
```

のようなbounded lookupが望ましい。

つまり、

```text
長時間探してmiss
```

より、

```text
短時間で見切ってcold prefill
```

の方がagent workloadでは安定する。

---

## 13. Siderostat

従来の `ds4-smart-proxy` は **`siderostat`** に名称変更された。

GitHub:

```text
okugauchi/siderostat
```

としてpublic公開を開始。

役割としては単なるreverse proxyではなく、

```text
Hermes / Codex
    ↓
Siderostat
    ↓
DS4 backends
```

のrequest routing / session affinity / health / retry / timeout制御を担う方向。

将来的にTP / distributed / RDMAを使い分ける場合は、

```text
backend capability
topology
transport
session locality
```

まで理解するルータへ発展させる余地がある。

特にTPが複数sessionを効率的に処理できる場合、

```text
in_flight >= 1
  = busy
```

という単純判定ではなく、

```text
active_sessions
context usage
queue depth
memory headroom
```

などを利用したroutingが有効になる。

---

## 14. 今後の注目ポイント

短期的には、現在のTCP over Thunderbolt + layer-parallel構成を維持するのが妥当。

今後、以下が揃ったタイミングで再評価する価値が高い。

```text
次期macOS
  RDMA over ThunderboltのGUI正式化

ds4
  PR #715相当のlayer-parallel RDMA
  TP fast sync
  ds4-server TP対応
  distributed DSpark

Siderostat
  topology-aware routing
```

特に評価すべき比較は、

```text
A. layer-parallel + TCP
B. layer-parallel + RDMA
C. TP + RDMA
D. layer-parallel + RDMA + DSpark
```

となる。

評価軸は、

```text
Prefill t/s
Decode t/s
TTFT
500K級contextでのtail latency
並列session性能
KV再利用率
transport overhead
```

が適切。

---

## 15. 総括

今回の調査から、ds4は単一ノードのDeepSeek V4 Flash専用ランタイムから、

```text
Apple Silicon
CUDA
ROCm
distributed pipeline
tensor parallel
RDMA
DSpark
DFlash系 speculative decoding
multi-session serving
```

を持つ、かなり高度な分散推論ランタイムへ急速に発展していることが確認できた。

現在のApple Silicon 2台構成に対しては、

```text
今:
  TCP over Thunderbolt
  layer-parallel

近い将来:
  RDMA layer-parallel

その後:
  TP + RDMA
  distributed DSpark
```

という段階的な移行が現実的。

次期macOSのRDMA正式化と、ds4側の関連PRのmergeが同じ時期に重なれば、2026年秋ごろが分散構成を再設計する良いタイミングになる可能性が高い。

## 16. upstream main 追跡更新（2026-08-24 JST）

固定 baseline `84cc882` から upstream `main` は 19 commit 進み、追跡時点の HEAD は
`c1d4597a80e300b803dc642519718f2c999589da`（2026-08-23）だった。本文の「近い将来」や
「進行中の PR」に関する記述を、現在の main の実装と混同しないため、以下を追記する。

### 16.1 現在の main に入った更新

- DSpark は非ゼロ温度の通常サンプリングにも接続され、通常の `--dspark` は
  opportunistic sampling を行う。DFlash の temperature-zero draft と target の
  continuation が一致した部分を直接採用するため、通常の温度サンプリングより決定的に
  なることがある。
- 出力分布を通常の target distribution に保つための `--mtp-exact-sampling` が追加された。
  greedy proposal を target probability で受理し、棄却時は残余分布から選択する方式で、
  デフォルト confidence threshold は `0.8`。`--temp 0` は fully greedy を指定する。
- これらの DSpark オプションは `ds4`、`ds4-agent`、非 batched の `ds4-server` で利用できる。
  `--batched-session` 中は speculative decoding が無効で、通常の target decoding になる。
- DeepSeek V4 PRO 0813 の q2 imatrix 取得対象と 100 ケースの品質 oracle が追加された。
  Flash 0731 の checkpoint が置き換わったわけではなく、PRO の別 checkpoint である。
- ROCm/Strix Halo 向けに routed-expert の native MXFP4 decode/prefill、occupancy variant、
  検証手順が追加された。これは Apple Metal の tensor parallel や layer-parallel RDMA の
  実装追加とは別の更新である。

### 16.2 TP / RDMA / distributed pipeline の現在地

今回の main 差分には、次の機能を production capability として追加する変更は確認できなかった。

- Apple 2台の layer-parallel activation transfer を TCP から RDMA に切り替える実装
- `ds4-server` または `ds4-agent` から Mac-to-Mac tensor parallel を起動・管理する実装
- distributed pipeline と DSpark の server lifecycle 統合

従って、本文の PR #715、#813、#835 などに基づく RDMA、server TP、distributed DSpark の
記述は、引き続き開発中・将来候補として扱う。本文に記載した Apple 2台の TCP
layer-parallel 測定値も、upstream main が RDMA 対応になった証拠ではない。

### 16.3 Siderostat の設計判断の更新

現在の Siderostat v0.3 については、次の判断を維持する。

| 項目 | 2026-08-24 時点の判断 |
| --- | --- |
| Apple 2台の実運用 | TCP layer-parallel を現行対象とする |
| Mac-to-Mac TP | `ds4` CLI の worker/coordinator 機能としてのみ把握し、`ds4-server` 管理対象にはしない |
| RDMA layer-parallel | upstream main 未実装。将来の採用候補 |
| distributed DSpark server | upstream main 未統合。将来の採用候補 |
| ROCm MXFP4 | ROCm/Strix Halo 固有の更新として追跡し、Apple 構成の用語・transport と混同しない |

したがって、本文の総括にある「近い将来 RDMA」「その後 TP + RDMA / distributed DSpark」は
ロードマップ上の仮説として残すが、現時点の実装済み機能や Siderostat v0.3 の受け入れ条件とは
分離して記録する。

### 16.4 追跡に使用した upstream の証跡

- [upstream main（追跡時点）](https://github.com/antirez/ds4/commit/c1d4597a80e300b803dc642519718f2c999589da)
- [ROCm MXFP4 kernel 実装](https://github.com/antirez/ds4/commit/39a8f18)
- [DSpark exact sampling](https://github.com/antirez/ds4/commit/769a8ba)
- [DeepSeek V4 PRO 0813 oracle](https://github.com/antirez/ds4/commit/3c63c06)
- [PRO 0813 model download target](https://github.com/antirez/ds4/commit/c35cf38)

## 17. upstream main 再確認と T-01 判定（2026-08-26 JST）

2026-08-26 に固定 baseline `84cc882` と upstream `main`
`c1d4597a80e300b803dc642519718f2c999589da`（2026-08-23）を再比較した。main は 19 commit
進んでいるが、Siderostat の管理対象を変更する機能追加は確認できなかった。

### 17.1 更新された領域

- DeepSeek V4 PRO 0813 q2 imatrix と 100 ケースの quality oracle が main に追加された。
- DSpark は非ゼロ温度の opportunistic sampling、`--mtp-exact-sampling`、非 batched
  `ds4-server` 接続を得た。`--batched-session` は従来どおり speculative decoding を使用しない。
- M5 Max 二台の pipeline 測定値と DeepSeek V4 PRO Q4 の二台 layer split が README に追加された。
  いずれも plain TCP の layer-parallel であり、TP／RDMA layer-parallel の main 採用ではない。

### 17.2 採用に至っていない領域

main の README、`ds4_tp.c`、`ds4_server.c` を照合した結果、Mac 間 TP は `ds4` CLI の
`--tensor-parallel --role coordinator|worker` に限られる。`ds4-server` に存在する
`--cuda-tensor-parallel` は単一 CUDA host の別機能であり、Mac 間 TP の server lifecycle を示さない。
Mac layer-parallel RDMA と distributed pipeline DSpark も main には入っていない。

関連 PR の公開状態は、#813（ROCm server TP）が Open、#754（CUDA network EP/TP）が Open、
#835（pipeline DSpark）が Open、#715（Apple layer-parallel RDMA）が Closed である。いずれも
main の採用証跡ではない。したがって「PR が存在する」ことを「Siderostat が利用可能」と扱わない。

### 17.3 T-01 の結論

TP／RDMA／distributed DSpark は v0.3.0 へ入れず、v0.4+ backlog へ延期する。再開条件は次の
5 gate とする。

1. 対象 backend の `ds4-server` role/lifecycle が upstream main、または承認済み固定 backport に入る。
2. HTTP contract（stream、cancel、disconnect、session/KV、tool/continuation）が確定し、CLI/PTY
   scrape を本番経路にしない。
3. 固定 source／binary／model digest で RDMA と明示的 TCP の 24時間 endurance を完了する。
4. mismatch、RDMA 不在、rank desync、peer loss を起動前／実行中に fail-closed で扱い、silent
   fallback を行わない。
5. child、peer control、admission、generation、orphan の rollback が現行 v0.3 gate を退行させない。

この判定により、v0.3.0 の config、manifest、state machine、metrics に TP/RDMA placeholder を
追加せず、現行の TCP layer-parallel baseline を維持する。詳細な backlog と追跡リンクは
`dwarfstar-rdma-tensor-parallel-2026-08-18.md` の第12節に固定した。

### 17.4 追跡証跡

- [upstream main commit `c1d4597`](https://github.com/antirez/ds4/commit/c1d4597a80e300b803dc642519718f2c999589da)
- [upstream main commit history](https://github.com/antirez/ds4/commits/main)
- [PR #813](https://github.com/antirez/ds4/pull/813) / [PR #754](https://github.com/antirez/ds4/pull/754) / [PR #835](https://github.com/antirez/ds4/pull/835) / [PR #715](https://github.com/antirez/ds4/pull/715)
