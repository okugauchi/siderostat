# siderostat メニューバーモニター 仕様

- 文書状態: 実装済み (Phase 1〜4) / Phase 5 は実機確認待ち
- 作成日: 2026-08-12
- 対象baseline: `develop` (`4faf6d5` / デスクトップ通知実装後)
- 前提: 本仕様は siderostat 本体の `docs/spec.md` で定義する target behavior を変更しない付加レイヤである。
  モニターは本体とは別プロセス・別 crate として動作し、本体への変更は「状態を公開するメトリクス追加」に限定する。

## 1. 目的

macOS のメニューバーに常駐するアイコン型モニターで、次を可視化する。

- siderostat の cluster mode / state / target readiness
- DS4 の prefill loading progress（チャンク進行・%・cached tokens）
- KV cache hit / miss の状況（hit tokens、load 時間、累計）
- generation（デコード）の直近 TPS

本体が DS4 child の stdout/stderr をパイプで直接読み取っている仕組み（ファイルを介さない低
オーバーヘッドなログパイプライン）を情報源の基盤とし、追加のファイル読取や外部ツールを
必要としない。

## 2. 位置づけと設計方針

- **別プロセス**: モニターは siderostat 本体と独立したプロセスとして動作する。本体の停止・
  クラッシュ・再起動はモニターに影響せず、モニターは offline 表示に遷移して動作を継続する。
- **別 crate**: 単一リポジトリを Cargo workspace で管理し、`siderostat`（本体）と
  `siderostat-monitor`（モニター）を別パッケージとして構成する。本体の依存と spec を汚さない。
- **通信は admin API のポーリングのみ**: 本体のプロセス内に UI を統合しない。
  tray-icon（NSStatusItem を操作）はモニター側の main thread で動かし、本体の Tokio runtime
  構造を変更しない。
- **表示はメニューバー**: デスクトップ通知（Notification Center）とは別レイヤ。常時表示の
  ステータス可視化が目的であり、通知の抑制・重複は行わない。

## 3. アーキテクチャ

### 3.1 プロセス構成

```text
┌──────────────────────────┐
│ siderostat (本体)          │ LaunchAgent (gui/<uid> ドメイン)
│  - DS4 child stdout/stderr │
│    をパイプで直接読取       │
│  - parse_ds4_log_event で  │
│    prefill / kv cache を   │
│    構造化イベント化         │
│  - admin API /metrics で   │
│    状態を公開              │
└───────────┬──────────────┘
            │ HTTP (loopback, admin_listen)
            ▼
┌──────────────────────────┐
│ siderostat-monitor        │ 別プロセス（ログイン項目 or 手動起動）
│  - tray-icon (NSStatusItem)│
│  - admin API ポーリング   │
│  - メニューバー表示        │
└──────────────────────────┘
```

### 3.2 通信仕様

- モニターは `GET {admin_listen}/metrics` をポーリングする。
- ポーリング間隔は既定 2 秒、設定で変更可能。
- 本体に到達できない場合（接続失敗・タイムアウト・HTTP エラー）は offline 状態とし、
  ポーリング間隔を 5 秒へバックオフする。本体復帰を自動検出する。
- 現状の `/metrics` は認証なしで公開されているため、モニターはトークンなしで取得する。
  将来 `/metrics` に認証が付く場合は、admin token を Bearer で送る設定を追加する。
- メニュー「siderostat(runtime) の再起動」は `POST {admin_listen}/cluster/restart` を
  Bearer（hex 形式の admin token）で呼ぶ。このエンドポイントは認証必須のため `admin_token`
  設定が必要。失敗時もモニターは継続し、結果をログに記録する。

## 4. データソース

### 4.1 現状の `/metrics`（Prometheus text）

モニターは既存 family から次を取得する。

| 値 | family |
|---|---|
| cluster mode | `ds4_proxy_cluster_mode` |
| cluster state | `ds4_proxy_cluster_state` |
| generation | `ds4_proxy_cluster_generation` |
| target readiness | `ds4_proxy_target_ready` |
| node_id | `ds4_proxy_cluster_*` の label |

### 4.2 prefill / KV cache メトリクス（本体側の将来拡張）

モニターの主要表示項目（prefill progress、KV cache hit）は、本体側に次のメトリクスを追加して
公開することを前提とする。本体側の実装は別タスクとし、本仕様はモニター側の表示仕様を定義する。

| 値 | family（案） | 型 |
|---|---|---|
| prefill 進行中フラグ | `ds4_proxy_ds4_prefill_active` | gauge |
| prefill current tokens | `ds4_proxy_ds4_prefill_current` | gauge |
| prefill total tokens | `ds4_proxy_ds4_prefill_total` | gauge |
| prefill percent | `ds4_proxy_ds4_prefill_percent` | gauge |
| prefill cached tokens | `ds4_proxy_ds4_prefill_cached` | gauge |
| KV cache hit 累計 | `ds4_proxy_ds4_kv_cache_hits_total` | counter |
| 直近 KV cache hit tokens | `ds4_proxy_ds4_kv_cache_hit_tokens` | gauge |
| 直近 KV cache load ms | `ds4_proxy_ds4_kv_cache_load_ms` | gauge |

### 4.3 DS4 ログフォーマット（実機ソースで確認済み）

本体の `parse_ds4_log_event` がこれらの行を認識して上記メトリクスへ変換する。

prefill progress（`DS4_LOG_PREFILL`、stderr）:

```text
MMDD HH:MM:SS ds4-server: chat ctx=0..9005:0 prefill chunk 4096/9005 (45.5%) chunk=... t/s avg=... t/s ...s
```

KV cache hit（`DS4_LOG_KVCACHE`、stderr）:

```text
ds4: kv cache hit text tokens=9005 text=... quant=... key=... load=... ms file=...
```

generation progress（`DS4_LOG_GENERATION`、stderr）:

```text
MMDD HH:MM:SS ds4-server: chat ctx=... gen=42 ... decoding chunk=... t/s avg=... t/s ...
```

注意: `server_log` 経由の行には先頭に `MMDD HH:MM:SS ` のタイムスタンプが付く。本体側の
パーサーはタイムスタンプを除去してから prefix 照合する必要がある（現状の `parse_ds4_log_event`
はタイムスタンプなし prefix を想定しており、`ds4-server: listening on` も同様に認識できない
潜在ギャップがある。本体拡張時に併せて修正する）。

## 5. 機能要件

### 5.1 メニューバーアイコン

- 常駐アイコンとして表示する。
- アイコンは cluster mode を「2つの円 + 任意の接続線」の描画で表現する。テキストの
  `solo` / `paired` / `dist` は表示しない。
  - 緑の円: 有効稼働状態（operating）
  - 赤の円: 非稼働状態（non-operating）
  - 常に2つの円を表示する。
  - mode → 描画の対応:
    - `solo`（solo-standalone）: 左が緑・右が赤、2つの円は直線で接続されていない
    - `paired`（paired-standalone）: 2つとも緑、2つの円は直線で接続されていない
    - `dist`（distributed-mxfp4）: 2つとも緑、2つの円を接続する1本の直線を描画
    - offline / 不明: 2つとも赤、接続線なし
- アイコンのタイトル（または代替テキスト）に状態を要約表示する:
  - 通常時: 空（mode はアイコン描画のみで表現）
  - prefill 進行中: `prefill 45%` のように % を優先表示
  - offline: `offline`
- ツールチップに詳細（node_id、state）を表示する（mode 短縮名は含めない）。

### 5.2 メニュー構成

```text
siderostat (ヘッダー: node_id)
────────────────────────
Mode:    solo-standalone
State:   solo-standalone-ready
Gen:     42
Target:  local-standalone (ready)
────────────────────────
Prefill: 4096/9005 (45.5%)   ← 進行中のみ
  cached: 0 tokens
────────────────────────
KV cache: hit tokens=9005 load=12.3ms
  total hits: 7
────────────────────────
Decode:  32.1 t/s (直近)   ← TPS 実装後
────────────────────────
siderostat(runtime) の再起動
Monitor を終了
```

- セクションは状態に応じて動的に表示する（prefill 非進行中は prefill 行を出さない）。
- 「Monitor を終了」はモニター自身だけを終了する。siderostat 本体には影響しない。
- 「siderostat(runtime) の再起動」は認証付き admin API `POST /cluster/restart` を呼び、
  本体の current profile child を再起動する。モニター自身は終了しない。
  `admin_token` 未設定・不正の場合は失敗し、メニュー表示には影響しない（ログに記録）。

### 5.3 offline 表示

- 本体に到達できない場合、アイコンを offline 表示にし、メニューに「siderostat に接続できません」
  を表示する。
- 本体復帰を 5 秒間隔ポーリングで自動検出する。

## 6. 設定

`monitor.toml`（または同等の設定ファイル）を利用者のホームディレクトリから読み込む。

```toml
admin_listen = "http://127.0.0.1:18081"   # 本体の admin_listen
poll_interval_secs = 2                     # ポーリング間隔
offline_backoff_secs = 5                   # offline 時のバックオフ
show_decode_tps = true                     # generation TPS の表示有無
admin_token = ""                           # hex 形式の admin token（再起動メニューで必須）
```

- `admin_token` は本体の `admin.key`（生 bytes）を hex エンコードした値で、
  `siderostat cluster restart` が送る Bearer と同一。未設定だと「siderostat(runtime) の
  再起動」は 401 で失敗する。

- 設定が存在しない場合は既定値で動作する。
- 未知フィールドは拒否する（本体 config と同じ `deny_unknown_fields` 方針）。

## 7. セキュリティ

- admin token は設定ファイルまたは環境変数から読み、ログ・メニューへ出力しない。
- 通信は loopback（admin_listen）限定とする。
- モニターのログに API レスポンス本文（cluster 状態の JSON）をそのまま出力しない。

## 8. 非機能要件

- **リソース**: tray-icon ベースで軽量に保つ。WebView 等の重いフレームワークを使わない。
- **失敗耐性**: 本体停止・ポーリング失敗・パース失敗のいずれでもモニターはクラッシュせず、
  offline / 不明表示へフォールバックする。
- **起動**: ログイン項目（LaunchAgent の `gui/<uid>` ドメイン）または手動起動に対応する。

## 9. ディレクトリ構成

単一リポジトリを Cargo workspace で管理する。本体はルートに維持し、モニターは `monitor/` に置く。

```text
siderostat/
├── Cargo.toml              # [workspace] members = ["monitor"]
├── src/                    # siderostat 本体（現状維持）
├── tests/
├── monitor/
│   ├── Cargo.toml          # siderostat-monitor
│   └── src/
│       ├── main.rs         # エントリポイント（tray-icon 起動、ポーリング開始）
│       ├── config.rs       # monitor.toml の読込・検証
│       ├── client.rs       # admin API クライアント
│       ├── metrics.rs      # Prometheus text パーサー
│       ├── state.rs        # 表示状態の保持・更新
│       └── tray.rs         # tray-icon によるメニューバー UI
└── docs/
    └── menu-bar-monitor-spec.md
```

## 10. テスト方針

- **Prometheus text パーサー**: 既存 `/metrics` の fixture と、prefill/cache family を含む
  fixture をパースするユニットテスト。未知 family は無視する。
- **状態更新ロジック**: パース結果から表示状態（prefill %、cache hit 累計、offline 判定）が
  正しく更新されることのユニットテスト。
- **UI**: tray-icon の表示は手動確認とする。CI ではコンパイルとパーサー/状態ロジックのみ
  テストする。
- 本体の統合テスト（`tests/phase*`）はモニターの有無に依存せず、workspace 全体の Required CI
  （`cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` /
  `cargo test --all-targets`）で両 crate を検証する。

## 11. リスク・注意点

1. **tray-icon の macOS 依存**: NSStatusItem は AppKit の main thread で操作する必要がある。
   モニターは別プロセスなので main thread を自由に使える（本体の Tokio runtime 構成には影響しない）。
   macOS バージョン更新時は tray-icon の互換性を確認する。
2. **DS4 ログフォーマット依存**: prefill / kv cache ログは DS4 実装の出力に依存する。
   認識できない行は fail-open（非表示）とし、promotion 等の本体判断には影響させない
   （本体 spec の「unknown DS4 log を推測しない」方針と整合）。
3. **`/metrics` の認証**: 現状は認証なし公開。モニターが loopback のみを対象とする前提で許容するが、
   本体側で認証を追加する場合はモニター設定に token を追加する。
4. **prefill progress の粒度**: DS4 の prefill ログはチャンク単位（例 4096 tokens ごと）で出る。
   ポーリング間隔 2 秒と合わせれば十分な更新頻度になる。

## 12. 実装フェーズ

| Phase | 内容 | 成果物 |
|---|---|---|
| 1 | workspace 化 + monitor crate skeleton | ルート `[workspace]`、`monitor/` |
| 2 | admin API クライアント + Prometheus text パーサー | `client.rs` / `metrics.rs` |
| 3 | tray-icon によるメニューバー UI | `tray.rs` / `main.rs` |
| 4 | 本体側の prefill / KV cache メトリクス追加 | `parse_ds4_log_event` 拡張、`metrics.rs` |
| 5 | 統合・動作確認（実 DS4 で prefill 中の表示確認） | 受け入れ証跡 |

Phase 1〜4 は 2026-08-12 に実装済みとする。
- Phase 1: workspace 化 + monitor crate skeleton（完了）
- Phase 2: `monitor/src/config.rs`、`metrics.rs`、`client.rs`、`state.rs`（完了）
- Phase 3: `monitor/src/tray.rs`、`main.rs`（完了）
- Phase 4: 本体 `parse_ds4_log_event` 拡張（prefill / kv cache / generation、タイムスタンプ除去）と
  `Metrics` の DS4 gauge 公開（完了）
- Phase 5: 実 DS4 での prefill 中の表示確認は実機が必要なため未実施。CI ではパーサー/状態ロジックの
  テストとコンパイルを検証済み（本体 170 tests、monitor 13 tests、Required CI 成功）
