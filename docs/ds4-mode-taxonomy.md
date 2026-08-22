# ds4-server 実行形態と Siderostat 用語の正規化

更新日: 2026-08-22

この文書は、[`docs/research/ds4-investigation-report-2026-08-21_22-07_to_2026-08-22.md`](research/ds4-investigation-report-2026-08-21_22-07_to_2026-08-22.md) と現行 Siderostat の実装を対応付ける。`MXFP4` はモデルの量子化であり、実行トポロジーや Siderostat の mode ではない。

## 1. 主体の用語

| 用語 | 正規の意味 | 混同してはならないもの |
|---|---|---|
| `Siderostat` | macOS のメニューバーアプリ | `siderostat-runtime`、`ds4-server` |
| `siderostat-runtime` | 常駐 supervisor。`ds4-server` の起動、停止、再起動、自動起動、クラスタ制御を担当 | 実際の推論プロセス |
| `ds4-server` | Siderostat が管理する推論プロセス。HTTP endpoint または分散推論の native protocol を提供 | Siderostat のアプリまたはノード |
| ノード | 1台の Mac と、その上の Siderostat / `siderostat-runtime` / `ds4-server` | `ds4-server` プロセスそのもの |

「推論サービス」は対象が `siderostat-runtime` なのか `ds4-server` なのか判別できないため、ユーザー向けのメニュー、通知、仕様では使用しない。

## 2. 「モード」と呼ぶ範囲

Siderostat の `mode` は、`ds4-server` の安定したリクエスト処理トポロジーとノード間構成を指す。起動中、停止中、接続待ち、復旧待ちなどは `ClusterState` であり、モードではない。

### 2.1 現行の安定モード

| 正規表示名 | machine name | `ds4-server` の実行形態 | 現行実装 |
|---|---|---|---|
| `Solo Standalone` | `solo-standalone` | 各ノードが自身の standalone profile で `ds4-server` を起動し、そのノードのリクエストを処理 | 実装済み |
| `Paired Standalone` | `paired-standalone` | peer を認証済みで検出した状態。coordinator の standalone `ds4-server` が処理し、worker の request は coordinator へ転送 | 実装済み |
| `Distributed (layer-parallel)` | `distributed-layer-parallel` | model layer を coordinator と worker の2つの `ds4-server` に分割し、1つの推論を順次処理。現行 transport は TCP over Thunderbolt | 実装済み |

旧 build の永続状態・control payload にある `distributed-mxfp4` は `distributed-layer-parallel` の旧 machine name として読み込む。新しく保存・送信する値には使用しない。

`Paired Standalone` は distributed inference ではない。2ノードがペアになっていても、model layer を2つの `ds4-server` に分割していなければ `Distributed (layer-parallel)` ではない。

### 2.2 ライフサイクル状態

| 分類 | `ClusterState` |
|---|---|
| 起動 | `Booting`, `SoloStandaloneStarting` |
| 単独提供 | `SoloStandaloneReady` |
| peer 検出・ペア形成 | `Pairing`, `PairedStandaloneReady` |
| distributed 昇格 | `AwaitingWorkerHello`, `Promoting`, `DistributedStarting`, `DistributedReady` |
| distributed からの降格 | `Demoting` |
| 復旧制御 | `Backoff`, `ManualInterventionRequired` |

通知やメニューで安定モードを表示する場合は `Solo Standalone`、`Paired Standalone`、`Distributed (layer-parallel)` を使う。`Starting`、`Pairing`、`Backoff` などは状態または操作結果として表示する。

## 3. モードとは別のモデル情報

モデルに属する情報は、実行トポロジーと同じ文字列へ詰め込まない。現行の config / manifest では次の項目を独立して管理する。

| 項目 | 例 | 意味 |
|---|---|---|
| model family / checkpoint | `DeepSeek V4 Flash` / `flash-0731` | どのモデル系列・checkpointか |
| quantization | `q2`, `q2-q4`, `mxfp4` | 重みの量子化分類。`MXFP4` はここに属する |
| speculative support | `none`, `dspark` | speculative decoding を利用するための support。DSpark はこの項目であり mode ではない |
| residency | `resident`, `ssd-streaming` | 重みのロード方式 |
| context / KV | `context_size`, `kv_disk_dir` | context 上限と KV cache の配置 |

現行 Siderostat の `[ds4.dspark]` は standalone model に対する DSpark support GGUF、confidence、strict を管理する独立設定である。DSpark が有効でも mode は `Solo Standalone` のままであり、「DSpark mode」とは呼ばない。

現行の分散構成は、次の組み合わせとして記録・表示する。

```text
mode / topology:       Distributed (layer-parallel)
model family:          DeepSeek V4 Flash
checkpoint:            flash-0731
quantization:          MXFP4
speculative support:   none
transport:             TCP over Thunderbolt
```

この組み合わせを `Distributed MXFP4` と短縮してはならない。quantization の変更が topology の変更を意味するわけではなく、同じ layer-parallel topology が別の quantization に対応する可能性もあるためである。

## 4. 実行トポロジー、通信、役割

### 4.1 実行トポロジー

| 正規名 | 内容 | 現状 |
|---|---|---|
| `Standalone` | 1つの `ds4-server` が model 全体を処理 | Siderostat で実装済み |
| `Layer-parallel distributed` | model layer を複数ノードへ分割し、token が stage を順番に通過 | 実装済み |
| `Tensor-parallel distributed` | 複数ノードが同じ token の処理に参加し、weights / routed experts を分担 | ds4 で進展。Siderostat 統合は未実装 |

`Layer-parallel distributed` と `Tensor-parallel distributed` は異なる実行形態であり、後者を現在の distributed profile の layer range 差し替えとして扱わない。

### 4.2 通信方式

通信方式は mode ではない。

| 正規名 | 用途 | 現状 |
|---|---|---|
| `local` | 同一ノード内の standalone `ds4-server` | 実装済み |
| `TCP over Thunderbolt` | layer-parallel の node 間 activation / control 通信 | 現行 distributed 構成 |
| `RDMA over Thunderbolt` | node 間通信の低遅延 transport | ds4 / macOS 側とも将来候補。Siderostat 未実装 |

### 4.3 ノードの役割

`Coordinator` と `Worker` は、layer-parallel の control-plane / request-routing role であり、mode ではない。

- `Coordinator`: coordinator-side HTTP ingress と local upstream を持ち、worker との制御・分散起動を管理する。
- `Worker`: coordinator の制御下で worker-side `ds4-server` を起動し、worker node の request を coordinator へ転送する。
- Tensor parallel では、両ノードが同じ token の処理に参加するため、Siderostat が `Coordinator/Worker` をそのまま表示名として再利用しない。

## 5. speculative decoding の分類

speculative decoding は model/profile に付随する strategy であり、mode ではない。

| 正規名 | 内容 | 現状 |
|---|---|---|
| `plain decode` | speculative decoding を使わない通常 decode | 実装済み |
| `DSpark` | support GGUF を使う verifier / speculative decoding | standalone で一部実装済み |
| `layer-parallel + DSpark` | layer-parallel と DSpark を組み合わせる構成 | ds4 で開発中。Siderostat 未実装 |
| `DFlash2` | draft model を使う block-diffusion 系 speculative decoding | ds4 で開発中。DeepSeek V4 Flash の現行経路ではない |

必要な場合は `Solo Standalone + DSpark`、`Distributed (layer-parallel) + DSpark` のように、mode / topology と strategy を組み合わせて表記する。`DSpark mode`、`DFlash2 mode`、`Distributed MXFP4 mode` という表記は追加しない。

## 6. 現行・将来の対応表

| 構成 | topology | transport | quantization | speculative support | Siderostat の扱い |
|---|---|---|---|---|---|
| 現行単独 | `Standalone` | `local` | profile による | `none` または `DSpark` | `Solo Standalone` |
| 現行ペア | `Standalone` を coordinator に集約 | TCP control / peer forwarding | profile による | `none` | `Paired Standalone` |
| 現行分散 | `Layer-parallel distributed` | TCP over Thunderbolt | `MXFP4` | `none` | `Distributed (layer-parallel)` |
| 将来の低遅延分散 | `Layer-parallel distributed` | RDMA over Thunderbolt | profile による | `none` または `DSpark` | transport variant 候補 |
| 将来の共同 token 処理 | `Tensor-parallel distributed` | TCP または RDMA | profile による | `none` または `DSpark` | 新しい topology / profile。未実装 |
| 将来の別 speculative path | 対応する topology に依存 | topology に依存 | profile による | `DFlash2` | strategy。未実装 |

Vision、1M context、multi-session serving、disk KV cache、routing policy は capability または運用特性であり、mode のバリエーションではない。

## 7. 表記規則

### ユーザー向け

- `ds4-serverがStandaloneモードで起動しました`
- `ネットワーク上の別のds4-serverを検出しました`
- `2台のds4-serverをDistributed（layer-parallel）モードに切り替えました`
- `ds4-serverのDistributed（layer-parallel）への切替に失敗しました。Standaloneで待機します`

### 内部識別子・metrics

新しい値は次を使用する。

- `solo-standalone`
- `paired-standalone`
- `distributed-layer-parallel`
- `distributed-ready` などの `ClusterState` 名
- standalone profile の `quantization` ラベル

`distributed-mxfp4` と `model_variant` は旧設定・旧状態の入力互換性のためだけに残す。内部識別子をユーザー向け表示へ出す場合は、正規表示名へ変換する。`distributed` 単独、`coordinator mode`、`worker mode`、`DSpark mode`、`推論サービス` は新規文言として追加しない。

## 8. 将来実装時の原則

1. 新しい ds4 実行形態を追加するときは、topology、role、transport、quantization、speculative support、profile compatibility を個別に定義する。
2. `StableMode` を増やすのは、request routing、lifecycle、persistent state、metrics、通知、rollback の全契約が実装された後にする。
3. TP は layer-parallel と別の topology / deployment として扱い、`--layers` の設定だけで表現しない。
4. RDMA は TCP と同一視せず、明示的な readiness、device、fallback policy、compatibility check を持たせる。
5. DSpark と DFlash2 は mode 名にせず、model binding と speculative strategy の検証結果として表示する。
