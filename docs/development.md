# 開発者向け開発手順

この文書は、siderostatの変更、ビルド、テスト、macOS 配布 artifact を扱う開発者向けの手順です。通常の利用者は
[README](../README.md)と[利用者向け導入ガイド](installation.md)を参照してください。

v0.3.0 の公式提供物はソースコードです。公式の事前ビルド済み `.app`、`.pkg`、DMG、
`Siderostat Uninstaller.app` は配布しません。利用者向けの導入経路も、ソース checkout で
`cargo xtask install --start` を実行する方法を正本とします。

`app-dev`、`pkg-dev`、`dmg-dev`、`sign` は macOS のローカル artifact 検証と将来の任意の
バイナリ配布を対象とする開発者向け workflow です。これらの署名・公証・timestamp は、
v0.3.0 のソースリリース受入条件ではありません。

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
起動項目を扱います。実機へインストールする場合は、[利用者向け導入ガイド](installation.md)の
手順と確認項目を先に確認してください。

配布 artifact の開発検証は次の順序で行う。

```sh
cargo xtask app-dev --version 0.3.0 --build-number <build> --verify
cargo xtask pkg-dev --app-dir build/app-dev --version 0.3.0 --output-dir dist
cargo xtask dmg-dev --app-dir build/app-dev --package dist/Siderostat-0.3.0.pkg \
  --version 0.3.0 --build-number <build> --output-dir dist --verify
```

Developer ID 署名、公証、staple は `cargo xtask sign` の承認済み手順だけを使う。
`--timestamp-mode none` の成果物は internal diagnostic artifact であり、配布物にしない。

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
- [内部導入・受け入れ手順](internal-installation.md): 開発者向けの実機導入と受け入れ確認
