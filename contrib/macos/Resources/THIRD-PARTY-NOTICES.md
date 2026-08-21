# Third-party notices

本アプリケーションは以下の third-party 依存を利用しています。依存の完全な
inventory（バージョン、ライセンス、ソース）はリリース時（R-02）に生成する
SBOM / dependency inventory を参照してください。

## Rust crate dependencies

Siderostat の runtime（`siderostat`）、monitor（`siderostat-monitor`）、および
build tooling（`xtask`）は、`Cargo.toml` / `Cargo.lock` に列挙された Rust crate
に依存しています。各 crate のライセンス条項はそれぞれのソース配布物に含まれる
LICENSE / COPYING ファイルを参照してください。本通知はリリース時に
`cargo license` 相当の inventory で確定します。

## DwarfStar / ds4-server

`ds4-server` は本アプリに同梱されず、外部 executable としてユーザーが選択し、
manifest で source commit と digest を検証します。そのライセンス条項は
`ds4-server` 側の配布物に従います。

## 注意

- 本ファイルはビルド時 resource として bundle に含まれます。
- 依存 inventory の正確な一覧とライセンス全文への link はリリース時に更新します。
