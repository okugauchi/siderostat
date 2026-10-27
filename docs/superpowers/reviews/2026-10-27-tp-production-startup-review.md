# TP 起動経路のレビューと検証

日付: 2026-10-27
対象: `fix/tp-production-wiring`、承認済み TP 通常起動経路の設計
範囲: 通常 serve の TP 配線、制御、child 監視、復帰、永続化、関連文書。モデル設定・実機導入・配布は対象外。

## レビュー結果

`siderostat-review` の7軸と既存レジストリを適用し、別エージェントによる独立レビュー、
修正、再レビューを実施した。以下の指摘を解消し、再レビューで未解消の重大指摘がないことを確認した。

| 指摘 | 修正と回帰検証 |
|---|---|
| 準備中 worker の異常終了を監視しない | Starting/Awaiting も監視し、owned effect の実行途中を観測しない。child 喪失・制御消失の回帰テスト。 |
| 生きた coordinator が開始を放棄すると worker が待ち続ける | 有効な同 session の Status だけで更新する起動期限を追加。開始放棄の回帰テスト。 |
| Ready 後の障害が試行上限を消費しない | session ごとに一度加算して永続化。3障害後の4回目拒否と再生成後の保持を検証。 |
| drain 失敗で session 番号を再利用する | 試行回数と session 番号を副作用より前に同時保存。 |
| 終了 cleanup が Standalone を再公開する | shutdown と fallback を分離。再起動後の cleanup でも受付を閉じたまま保持。 |
| 手動 reconcile が状態イベントだけで TP Ready に戻す | 実停止と Standalone 起動確認を通す。budget は解除しない。 |
| 停止失敗で child の所有 handle を失う | SupervisedSlot に handle を戻す。所有 shell child の停止失敗試験と統合復帰試験。 |
| worker の paired 転送先が reducer と異なる | snapshot の Coordinator target に統一し、実 HTTP 統合試験で assert。 |
| TP child が診断に現れない | 実 lifecycle slot・制御 lease・phase を診断へ反映。 |
| 文書の topology 値と実装が異なる | 両言語を `tensor-parallel` に統一し、起動 rollback と稼働中 auto_demote を区別。 |

## 7軸の確認

| 軸 | 確認内容 |
|---|---|
| 仕様整合 | worker Prepared → coordinator 起動 → handshake/HTTP → 内部生成 → 公開。schema 3 と承認済 capability/role artifact を使用。 |
| 影響範囲 | 既定 LP と v1 を保持。runtime 設定、ネットワーク、secret、モデル、サービスに変更を加えていない。 |
| 抽象化 | 本番は ManagedChild、試験は注入 lifecycle。同じ署名付き制御 HTTP と reducer を通し、dry-run の実 child はない。 |
| 失敗・rollback | 開始失敗、準備中・稼働中の喪失、Force、終了、手動復帰を確認。停止失敗時は受付を閉じ所有権を保持。 |
| 試験証跡 | 下記の実行結果と個別回帰試験。試験は ephemeral port と専用 temp state を使用。 |
| 設定・文書 | README、operations、troubleshooting の英日対応を確認。LP layers、DSpark/SSD、TCP、管理引数 override を拒否。 |
| 秘密・成果物 | 合成 fixture と固定内部 warm-up のみ。raw model output をログへ転記しない。secret/model/runtime artifact の追加なし。 |

## 検証証跡

- `cargo fmt --check`: 成功。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: 成功。
- `cargo test --all-targets`: 653件成功。
- `cargo test --test reconnect_production --features test-support -- --test-threads=8`: 41件成功。
- `cargo test --test tp_production_http --test tp_production_startup --features test-support`: 25件成功（HTTP 18件、起動契約7件）。
- `cargo test --lib system_gid_matching_requires_exact_index_and_address --all-features`: 成功。GID index と IPv4 の完全一致、空白、IPv6 hex 表記、不一致 suffix を確認。
- `cargo test --workspace --all-targets --all-features`: 最終差分で1060件成功（68 suites、exit 0）。
- `git diff --check`: 成功。

検証中に既存の cross-kind 並行取得を前提とした試験を更新した。LP の通常起動と再接続 suite は維持する。
独立レビューの最初の2件の再現試験は失敗を確認してから恒久回帰試験へ移した。

## 実機受入の境界

これはソフトウェアの起動・監視・復帰経路の受入である。過去の上流 ds4 直接起動試験は
この経路を実証しない。2台の実 Mac による通常起動、RDMA 接続、生成、切断・再接続、
Force、再起動の受入は未実施であり、導入前の別作業とする。特定モデルの設定・互換性保証も含まない。

今回得た一般化できる観点はレビュー registry の「非同期 lifecycle と失敗回数の保持」へ追記した。

## 実機導入時の追加修正

1017 Q2 / M5 Max + M4 Max の上流直接RDMA試験で、`ds4_log` がboundメッセージに改行を
付けず次のstderrメッセージへ連結することを確認した。build32の行末一致では接続完了を認識できない。
実ログの連結形でREDを確認し、rank 0/1・50/50 ownership・RDMAの完全markerに一致する検出へ修正した。
peer接続観測、sessionとchildの照合、truncated除外、障害判定の保持、HTTP/warm-up gateは維持。
関連26テストとworkspace clippyに成功し、独立再レビューでも重大指摘なし。
上流直接RDMAでは両ノードの接続と短い生成に成功。通常siDeroStat経路の実機受入は別途継続する。
