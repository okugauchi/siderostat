# 開発者向け開発手順

この文書は、siderostatの変更、ビルド、テストを行う開発者向けの手順です。通常の利用者は
[README](../README.md)と[導入ガイド](installation.md)を参照してください。

## 必要な環境

- Rust安定版
- Rust 2024 Editionを利用できる環境
- macOSでの実機確認を行う場合は、対応するApple Silicon Mac

## ビルドと検証

リリース用バイナリを作成する場合は、次を実行します。

```sh
cargo build --release
cargo build --release -p siderostat-monitor
```

変更を提出する前に、次の検証を実行します。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
git diff --check
```

インストールを含む検証は、次のコマンドでまとめて実行できます。

```sh
cargo xtask install --ci
```

`cargo xtask install` は実行ファイル、設定、秘密情報、モデルのマニフェスト、macOSの
起動項目を扱います。実機へインストールする場合は、[導入ガイド](installation.md)の
手順と確認項目を先に確認してください。

## テストの考え方

通常のRustテストは標準の並列実行を使用します。テストはポート番号、状態ファイル、
模擬プロセスのライフサイクルをテストごとに分離し、実行順序や固定待機時間に依存させません。

再接続処理の並列検証を明示的に行う場合は、次を使用します。

```sh
cargo test --test reconnect_production --features test-support -- --test-threads=8
```

## 関連文書

- [貢献ガイド](../CONTRIBUTING.md): ブランチ、コミット、レビュー、検証の運用方針
- [詳細仕様](spec.md): 状態機械、通信、設定、互換性の仕様
- [導入ガイド](installation.md): 実機導入と受け入れ確認
