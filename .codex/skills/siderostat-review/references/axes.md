# 汎用レビュー観点（7 軸）

CONTRIBUTING.md の Review 基準と、これまでの開発項目（cluster dry-run 等）のレビューから一般化した 7 つの観点。どの開発項目にも適用する。

## 軸1: Target behavior と仕様書の整合
- 変更が仕様書（docs/spec.md 等）の対象動作と一致するか
- 新設の CLI フラグ / 設定が正しく配線され、ServeOptions 等を経由して実体へ到達するか
- 目的を production 実装の書き換えなしで実現しているか（専用の別経路を不必要に新設していないか）

## 軸2: 影響範囲の封じ込め（スコープ外リソース非操作）
- 変更がスコープ外の実リソース（実プロセス、永続 StateStore、外部サービス）を絶対に触らないか
- 通常経路（例: 通常の new()）が新モードに誤って混入しないか
- 新モードが既定値で有効にならないか（明示フラグでのみ有効）

## 軸3: 抽象化・シミュレーションの整合性
- 新設する抽象 / モック / シミュレーション（合成フレーム・route probe 等）が、本番経路と同一の前提条件・式に従うか
- シミュレーションが本番の control plane / state machine を誤って書き換えないか
- 合成データへの参照管理（Weak 参照等）が runtime の lifetime を延長しないか

## 軸4: Failure behavior と rollback 可能性
- 失敗時（timeout、lease lost 等）が production と同じ failure action（backoff / manual / paired / solo）に従うか
- rollback が「変更を止めるだけ」で済み、実状態を汚染しないか
- 変更が残す状態が後続の本番起動を誤導しないか

## 軸5: Test evidence
- 既存テストが回帰しないこと（cargo test --all-targets）
- 変更固有の unit / integration / actual test が存在し、目的（例: 実プロセス非操作）を検証するか
- テストが実リソースを汚染しないか（port・state・child lifecycle をテスト単位で分離し、固定 sleep / 実行順依存を作らない）

## 軸6: Config・運用・README・導入ガイドへの影響
- 新フラグ / 設定が本番設定・運用ドキュメントを誤って変更しないか
- README（英語版正本）・導入ガイドに用途と制約が明記されるか（日本語版との対応を確認）
- 新機能が既定値で有効にならないことをドキュメント面でも確認する

## 軸7: Secret・model・runtime artifact の非混入
- 実 model / checkpoint / secret / KV cache を読み書きしないか
- 通信フレームに secret / prompt が含まれないか
- ログに秘密値・prompt・session identifier を出さないか

## 完了条件
- 全軸で指摘が解消され、再レビューが PASS している
- `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets` が PASS
- 影響範囲の封じ込めがコード・テストで確認できる
