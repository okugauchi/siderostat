# tray-min: 最小の tray-icon 実験 (macOS)

## 目的

`target/release/siderostat-monitor` がメニューバーに何も表示されない問題を、
siderostat本体から独立して切り分けるための最小実験。

- 使う crate: `tray-icon 0.24` (+ `objc2-app-kit 0.3`)
- この macOS (26.6.1) でコンパイル可能かを確認済み (release ビルド成功)。
- 本体 `monitor/src/main.rs` が欠いている「AppKit イベントループ」を含む、
  正しい起動パターンを示す。

## 正しいパターン (tray-icon README の macOS 要件)

tray-icon は macOS では **main thread でイベントループが動作していること** を要求する。
`main.rs` は以下を行う:

1. `NSApplication::sharedApplication(...)` で AppKit アプリを生成。
2. `setActivationPolicy(Accessory)` でバックグラウンド (メニューバーのみ) アプリにする。
3. `TrayIconBuilder` でステータスアイテムを作成。
4. `app.run()` で AppKit のメインイベントループを回す。

siderostat-monitor の `main.rs` は tray を作るが `app.run()` を呼ばず、
`thread::sleep(500ms)` ループで回っているため、ステータスアイテムは作成される
(System Settings の「メニューバー」一覧に載る) が実際には描画されない。

## 実行手順 (実GUIセッションで実行し、メニューバーを目視)

```sh
./target/release/tray-min
```

- メニューバーに青い四角アイコンが表示されれば crate は使用可能 → モニタを修正する。
- 「終了」メニューでプロセスが終了する。
- 表示されなければ macOS 26.6.1 での tray-icon/NSMenuBar 非対応を疑う。

## 注意

この実行環境 (サンドボックス) では GUI プロセスを起動できないため、目視確認は
利用者の実セッションで行う。
