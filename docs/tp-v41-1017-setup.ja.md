# V4.1 Flash 1017 Q2：モデル設定とダウンロード

確認日: 2026-10-27。対象は Mac Studio M4 Max / 128GB と MacBook Pro M5 Max / 128GB の2台。
初回はテキスト推論だけを確認し、Vision・DSparkは使わない。

## 今回用意した設定

| 項目 | 初回検証値 |
|---|---|
| モデル | DeepSeek V4.1 Flash **1017**、DwarfStar専用 Q2 |
| Mac Studio | coordinator、制御 Bridge `10.99.0.1` |
| MacBook Pro | worker、制御 Bridge `10.99.0.2` |
| Distributed | `tensor-parallel` / `rdma`、resident expert shard |
| Standalone | 同じ1017 Q2、`ssd-streaming`、expert cache `16GB` |
| Context | 両モード・両ノードとも8,192 |
| 同時要求 | 1 |
| 自動pair・自動promotion | 準備中は両方無効 |
| 自動demotion | 有効 |
| 生成速度低下に対する自動復旧 | 無効 |

- [Mac Studio用設定](../examples/tp-v41-1017/macstudio.toml)
- [MacBook Pro用設定](../examples/tp-v41-1017/macbookpro.toml)
- [配布revision・サイズ・SHA-256の固定値](../examples/tp-v41-1017/model-lock.json)

これらは導入候補であり、稼働中の `~/Library/Application Support/siderostat/config.toml` は変更していない。
モデルと実測済みmanifestがない状態では起動できない。`auto_promote = false` は未検証manifestを
使って起動できる設定ではなく、準備後も意図しないTP起動を防ぐためのもの。

TPではモデル全体を2分割したファイルを用意するのではなく、**両Macに同じGGUFファイル全体**を置く。
上流はV4.1 Q2の2台128GB Macでのresident TPを案内しており、Engram tableはディスク上に保持する。
これは `--ssd-streaming` でexpertを読み出す単独運用とは別の仕組みである。
128GBの単独Macではmain weights全部をresidentにできないため、縮退先にはSSD streamingを指定した。
この異種M4/M5構成と1017ファイルの組合せの受入は、これから実施する。
[上流分散ガイド](https://github.com/antirez/ds4/blob/ee7e8405232c8971000cc7ba2b945b00bf6d20d3/docs/DISTRIBUTED.md)、
[配布元モデルカード](https://huggingface.co/antirez/deepseek-v4.1-flash-gguf)

## 1. 取得するファイル

今回使うのは次の**1ファイル**。旧V4.1 Q2、0731、Q4、MXFP4、他runtime向けGGUFと混在させない。

```text
Repository: antirez/deepseek-v4.1-flash-gguf
Revision:   9dcee51692e6540f697141fc98b097e3c96492a3
Filename:   DeepSeek-V4.1-Flash-1017-IQ2_XXS-Q2_K-imatrix.gguf
Bytes:      365713686528
SHA-256:    5296e85ff64b42ecb62c8d0c0c54e1caa07e3df8e93112f2365ebbaed62ce93a
```

配布元のrevision固定APIでファイルのsizeとLFS SHA-256を確認した。
[配布メタデータ](https://huggingface.co/api/models/antirez/deepseek-v4.1-flash-gguf/revision/9dcee51692e6540f697141fc98b097e3c96492a3?blobs=true)

365.7 GB（約340.60 GiB）のファイルを各Macに1個置く。モデルだけで両Mac合計約731.4 GBになる。
今回の予算は**各Macで450 GB以上の空き**を目安にする。これはモデル、Standalone用KV cache 16GiB、
取得中の管理データと余裕を見込む初期作業予算であり、OSの空き領域まで使い切らないこと。
TP側の `kv_disk_*` は現builderが起動引数へ渡していないため、TPのディスク使用上限を保証しない。
別の場所でダウンロードしてからコピーする場合は、その分の追加空きが必要。
高速なローカルSSD上の通常ファイルを使う。ネットワーク共有やsymlinkをモデルパスに指定しない。

この検証ではVision encoderとDSpark supportモデルはダウンロード不要。

## 2. ダウンロード（両Macで実施）

`hf --help` が使える場合はインストール不要。未導入でHomebrewを利用している場合は次を実行する。

```sh
brew install hf
hf --help
```

[Hugging Face公式CLIガイド](https://huggingface.co/docs/huggingface_hub/guides/cli)

以下を**Mac StudioとMacBook Proそれぞれ**で実行する。保存先は候補設定と一致している。
この手順をガイド作成時に実行したわけではない。

```sh
MODEL_DIR="$HOME/LLM/models/ds4"
mkdir -p "$MODEL_DIR"
df -h "$MODEL_DIR"

hf download antirez/deepseek-v4.1-flash-gguf \
  DeepSeek-V4.1-Flash-1017-IQ2_XXS-Q2_K-imatrix.gguf \
  --revision 9dcee51692e6540f697141fc98b097e3c96492a3 \
  --local-dir "$MODEL_DIR"
```

途中で中断した場合は、同じrevision・同じ保存先で同じコマンドを再実行する。
再利用情報を失わないよう、取得途中の `.cache` や一時ファイルを削除しない。
公開モデルなので、まずログインせずに取得できるか確認する。認証を求められた場合は `hf auth login` を使い、
tokenをコマンドの引数やチャットへ貼らない。

片方でダウンロード後にもう片方へコピーしてもよい。その場合もコピー先で下記のサイズとSHA-256を確認する。
同じrevisionを指定したダウンロードは、両Mac間で同一ファイルを用意するための最も単純な方法。

## 3. 完了後の検証（両Macで実施）

```sh
MODEL_DIR="$HOME/LLM/models/ds4"
MODEL_FILE="DeepSeek-V4.1-Flash-1017-IQ2_XXS-Q2_K-imatrix.gguf"
(
  set -eu
  cd "$MODEL_DIR"
  test -f "$MODEL_FILE"
  test ! -L "$MODEL_FILE"
  test "$(stat -f '%z' "$MODEL_FILE")" = 365713686528
  printf '%s  %s\n' \
    5296e85ff64b42ecb62c8d0c0c54e1caa07e3df8e93112f2365ebbaed62ce93a \
    "$MODEL_FILE" | shasum -a 256 -c -
)
```

macOS用のサイズ確認である。SHA-256はファイル全体を読むため時間がかかる。
両Macで `…gguf: OK` になることが取得完了条件。サイズが合っていてもhashが違えば起動へ進まない。
この検査はモデルファイルの整合性を確認するもので、推論・RDMA動作の合格を意味しない。

監視メモやPRには旧checksumの不一致報告があるが、今回固定するds4 commitの
`download_model.sh` の実コードでは、上記size/SHA-256と一致した。
本ガイドは配布repositoryの変更や旧checkoutとの差を避けるため、HF revisionとファイル名を直接固定する。
[固定commitのダウンローダー](https://github.com/antirez/ds4/blob/ee7e8405232c8971000cc7ba2b945b00bf6d20d3/download_model.sh)

## 4. 設定を有効にする前の残作業

モデル取得後は、次の順で進める。今は候補設定を稼働中configへコピーしたり、サービスを再起動したりしない。
既存のruntimeと同じ公開portを使うため、第二のruntimeを並行起動することも避ける。

1. **上流source/buildを固定する。** 今回の候補は `ee7e8405232c8971000cc7ba2b945b00bf6d20d3`。
   確認したローカル `~/LLM/ds4` は10月7日の `fc80bd695da76ee14bcb86820bf900a5cdfda806` なので、
   そこを上書きせず、両Macの `~/LLM/ds4-tp-v41-1017` に隔離した同一commitを準備する。
   worker用 `ds4` とcoordinator/Standalone用 `ds4-server` の両方をbuildして、binary/help digestを採取する。
2. **各MacでStandaloneを実測する。** 同じ1017 Q2をSSD streamingで起動し、モデル認識と短い生成を検証する。
   生成内容・速度・メモリを確認し、縮退先として使えることを先に確定する。
3. **RDMAの実値を再確認する。** 候補値は過去H03の記録に基づく。
   Studio: `en5 / 10.99.10.1 / rdma_en5 / GID 2`、
   MacBook Pro: `en2 / 10.99.10.2 / rdma_en2 / GID 2`。
   現在のmember、device、GID、routeの確認が必要。Bridgeの制御アドレス `.0.1/.0.2` と混同しない。
   この作業ではalias追加、RDMA有効化、wired-memory上限の変更を行っていない。
4. **実測に基づくmetadataを準備する。** Standalone schema 2、TP deployment schema 3、
   各role artifact、source ancestry、help、モデル互換性・生成smokeを確認したcapabilityが必要。
   `model-lock.json` はダウンロードの固定値であり、runtimeが読むcapability/deployment manifestの代わりにはならない。
   必要な検証前に `verification.verified = true` を記入してgateを迂回しない。
   個別buildのrole binaryが異なる場合は共有deploymentのrole別許可集合を作り、両Macのdeployment ID一致も確認する。
5. **siDeroStatを導入して制御下で切り替える。** 使用するruntimeにはTP起動経路修正 `ed2ac19` が必要。
   上記metadataと既存secretを確認してから候補設定を採用する。
   TP試験時には両ノードの `auto_promote = true` が必要（現実装は手動promotionでもこの設定を確認する）。
   `auto_pair = false` を維持した制御下の開始確認を先に行い、自動接続試験はその後に有効化する。
6. **実機RDMA受入。** 両child、handshake、HTTP、内部warm-up、実生成、切断・再接続、Force、再起動を確認する。
   過去H03は0731の上流直接起動試験であり、今回の1017と通常siDeroStat起動経路の合格証跡にはしない。

候補設定のstate/manifests/KV cacheは既存運用から分離した。secretのパスは既存のものを参照するが、
secretの値は設定ファイルへコピーしていない。実際の導入時に両端の共有secretとnode-local admin tokenを確認する。

## 今回の確認範囲

- 配布revisionのAPIでsize/LFS SHA-256を照合し、固定した上流ダウンローダーの値とも一致を確認。
- 両候補設定を実際の `ModeAwareConfig` でparse・path展開し、実command builderで
  StandaloneのSSD cache指定、TPの8K/RDMA、SSD/DSpark/layersの非混入を確認。
- 実ファイルの必要箇所を隔離した一時fixtureへ置き換え、両設定の構造・設定値のvalidationが成功。
  実モデル・実manifest・RDMAの検証成功を意味しない。一時fixtureと確認用ファイルは削除済み。
- 設定・TP起動契約の既存回帰試験12件、fmt、workspace clippy、shell構文、ローカルリンク確認が成功。
- 別エージェントによる設定・手順レビューを実施し、未解消の指摘なし。
- モデル本体の取得、source/build更新、metadata生成、稼働設定の切替え、実機受入は未実施。
