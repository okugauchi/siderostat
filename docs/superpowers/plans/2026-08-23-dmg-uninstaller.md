# E-06: リリース DMG と GUI アンインストーラー

## 目的

配布仕様 `docs/distribution/macos-app-bundle-pkg-spec.md` の 2.3、14、15、16.1、16.5
を実装し、エンドユーザーが DMG 内の `Siderostat Uninstaller.app` を Finder から起動するだけで、
既存の Siderostat の常駐サービスとアプリ bundle を安全に整理できるようにする。

## 実装方針

1. インストール済み `Siderostat.app` の Monitor バイナリに、通常のメニューバー起動とは分離した
   `--unregister-services` モードを追加する。`SMAppService` で runtime LaunchAgent と main app
   login item を解除し、二重実行を成功扱いにする。
2. `Siderostat Uninstaller.app` は同じ Monitor 実行基盤の専用 bundle とし、Finder 起動時は
   `NSAlert` の確認 UI を表示する。確認後、インストール済み app のサービス解除、対象 PID の
   identity 確認付き停止、Finder の Trash 移動、正確な package receipt の整理を行う。
3. プロセス停止は固定された executable path と PID/command の一致を必須にし、`killall` や
   `sudo rm -rf` は使用しない。Application Support、secret、manifest、cluster state、model、
   KV cache は読み取りもしくは保持だけにする。
4. `xtask` に Uninstaller bundle と DMG の deterministic builder を追加する。DMG の直下は
   対応する `.pkg`、`Siderostat Uninstaller.app`、README だけとする。
5. 既存 `xtask sign` に `--with-dmg` を追加し、Uninstaller を Developer ID Application で
   署名後に DMG を作成し、DMG を notarize/staple/validate する。既存 pkg の署名・公証契約と
   metadata を維持し、DMG/Uninstaller の checksum と submission ID を追加する。

## テスト先行の作業単位

- RED: Service Management の全 service 解除順序/idempotency、Uninstaller bundle 判定、
  process list の exact identity、receipt ID、DMG file list の pure helper test を追加する。
- GREEN: 上記 pure helper と app-side `--unregister-services` を実装する。
- RED/GREEN: Uninstaller GUI、Finder Trash、receipt cleanup、child/runtime の停止確認を接続する。
- RED/GREEN: `xtask` の Uninstaller.app/DMG builder、`--with-dmg` dry-run と metadata を実装する。
- REFACTOR: i18n、README/installation/operations、エラーメッセージ、既存 E-02/E-05 との互換性を確認する。

## 検証

ローカルでは `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、
`cargo test --all-targets`、`git diff --check` を実行する。macOS では app/pkg/DMG の
`codesign`、`pkgutil`、`spctl`、`stapler`、`hdiutil` を実行し、最後に両ノードで
clean install → DMG Uninstaller → data 保持確認 → 再インストールを行う。
