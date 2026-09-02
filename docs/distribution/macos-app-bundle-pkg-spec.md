# Siderostat macOS Application Bundle / Installer Package 仕様

- **現行方針（2026-08-26）**: v0.3.1 はソース公開のみであり、公式のバイナリ、`.pkg`、DMG、
  `Siderostat Uninstaller.app` は配布しない。本書は既存の macOS bundle/package 実装と実機検証を
  説明する内部仕様、および将来の任意バイナリ配布・ローカル検証用の設計として保持する。
  本書に記載された Developer ID 署名、公証、secure timestamp、staple、Gatekeeper の条件は、
  v0.3.1 のソースリリース受入条件ではない。

- 文書状態: 配布構成・アンインストール構成 合意済み（実装・実機受入済み）
- 作成日: 2026-08-18
- 対象: Siderostat 次期配布形式
- 配布チャネル: Mac App Store 外の Developer ID 配布
- 関連仕様: [`docs/menu-bar-monitor-spec.md`](../menu-bar-monitor-spec.md)

## 1. 目的

現在の `/usr/local/bin` と `~/Library/LaunchAgents` を直接構成するインストール方式を、
macOS の標準的な `.app` と署名済み `.pkg` に置き換える。

本仕様は、次を定義する。

- monitor と runtime のどちらを main app / Helper とするか
- `Siderostat.app` の bundle layout
- runtime の常駐、登録、停止、更新方法
- `.pkg` の payload、署名、公証、upgrade 方針
- リリース DMG と `Siderostat Uninstaller.app` の提供形態
- 既存インストールからの移行
- security と acceptance criteria

## 2. 決定事項

### 2.1 推奨構成

**monitor を main application、runtime を同梱 LaunchAgent Helper とする。**

| component | packaging 上の役割 | process lifecycle |
|---|---|---|
| monitor | `Siderostat.app` の main executable | メニューバー UI。利用者が終了可能 |
| runtime | app bundle 内の LaunchAgent Helper | monitor とは独立して常駐。crash 時は launchd が再起動 |
| `ds4` / `ds4-server` | baseline では外部の manifest-approved executable | runtime が child として管理 |
| GGUF | app / pkg に同梱しない | 利用者が指定し、manifest で検証 |

この構成でも、runtime が製品機能の中心である点は変わらない。「Helper」は機能的重要度では
なく、macOS bundle と Service Management における lifecycle 上の役割を指す。

### 2.2 runtime を Helper とする理由

- `.app` を Finder から起動した際に必要なのは、設定、承認状態、稼働状態を見せる UI である。
- runtime は window や menu bar を持たず、logged-in user の background service として動く。
- monitor を終了しても推論 service を継続できるという現行の責務分離を維持できる。
- runtime を system-wide daemon にする必要がなく、root、setuid、privileged helper を避けられる。
- macOS 13 以降の `SMAppService` で、app bundle 内の LaunchAgent と System Settings の
  Background Items 表示を関連付けられる。

runtime を main executable、monitor を別 LoginItem app とする案は採用しない。この案では、
Finder からの起動、初回承認、設定 UI、障害時の説明を UI のない process が背負う一方、
結局 monitor 用の第二 bundle が必要になり、責務と署名単位が複雑になる。

### 2.3 将来の任意バイナリ配布単位

この節は、将来 v0.4+ 以降で公式バイナリ配布を採用する場合、またはローカルで package
workflow を検証する場合の設計である。v0.3.1 の公式導入は source checkout からの
`cargo xtask install --start` であり、以下の DMG、`.pkg`、Uninstaller は配布しない。

バイナリ配布を採用する場合の候補は、次のファイルを収録した署名・公証済み DMG とする。

```text
Siderostat-<version>.dmg
├── Siderostat-<version>.pkg
├── Siderostat Uninstaller.app
└── README.html
```

- `.pkg` の payload は `/Applications/Siderostat.app` 一項目だけとする。
- `Siderostat Uninstaller.app` は `.pkg` の payload に含めず、DMG から必要時に起動する。
- Uninstaller は Finder から起動できる GUI application とし、Terminal で shell script を
  実行させることを通常の利用者向け手順にしない。
- Uninstaller.app は Developer ID Application で署名し、notarytool へ一時 zip として提出して
  ticket を staple する。zip は配布 DMG には含めない。
- DMG 自体も Developer ID Application で署名してから公証し、staple 後に
  `spctl --assess --type open --context context:primary-signature` で検証する。
- DMG は `/Applications/Siderostat/` のような新しいインストールフォルダを作らない。
  既存の app bundle path、bundle identifier、upgrade/rollback 契約を維持する。

## 3. 適用範囲と非目標

### 3.1 対象

- Apple silicon 向け `.app`
- Developer ID Application / Installer による署名
- Hardened Runtime
- Apple notarization と stapling
- `/Applications/Siderostat.app` を配置する flat installer package
- per-user LaunchAgent と login start
- 現行の user configuration、secret、manifest、cache の維持

### 3.2 初期版の非目標

- Mac App Store 配布
- App Sandbox
- privileged LaunchDaemon / `SMJobBless`
- GGUF の同梱
- DwarfStar executable の自動ダウンロードまたは自動更新
- 自動 update framework
- `.pkg` 自体に uninstall UI を埋め込むこと
- Intel Mac 向け universal binary

App Sandbox は Mac App Store 外では必須ではない。Siderostat runtime は user-selected model、
外部 `ds4` executable、network、child process、既存 Application Support を扱うため、初期版では
Hardened Runtime のみを有効にし、App Sandbox は無効とする。例外 entitlement は必要性を
実機で証明したものだけ追加する。

## 4. process architecture

```text
┌─────────────────────────────────────────────────────────┐
│ /Applications/Siderostat.app                            │
│                                                         │
│  Siderostat (monitor / main app, LSUIElement)            │
│    ├─ ServiceManagement: runtime 登録・状態表示           │
│    ├─ admin API polling                                  │
│    └─ menu bar から開始・停止・再起動・設定               │
│                                                         │
│  siderostat-runtime (embedded LaunchAgent Helper)        │
│    ├─ public/admin/control HTTP                          │
│    ├─ cluster state machine                              │
│    └─ manifest-approved ds4 / ds4-server child           │
└─────────────────────────────────────────────────────────┘
                 │
                 │ model / config / state は bundle 外
                 ▼
~/Library/Application Support/siderostat/
~/Library/Caches/siderostat/
~/Library/Caches/ds4-kv/
~/Library/Logs/siderostat/
```

monitor の crash または通常終了は `siderostat-runtime` を終了させない。`siderostat-runtime` の crash は launchd が
再起動する。一方、利用者が menu の「siderostat-runtimeを停止して自動起動を無効化」を選んだ場合は、
`siderostat-runtime` を quiesce して `ds4-server` child を graceful stop した後に LaunchAgent を unregister する。
`SMAppService.unregister()` が実行中の LaunchAgent を終了し、将来の起動も停止するため、
この menu 操作は現在の `siderostat-runtime` と `ds4-server` の停止も伴う。

## 5. bundle 仕様

### 5.1 layout

```text
Siderostat.app/
└── Contents/
    ├── Info.plist
    ├── MacOS/
    │   └── Siderostat
    ├── Helpers/
    │   └── siderostat-runtime
    ├── Library/
    │   └── LaunchAgents/
    │       └── dev.siderostat-ds4-proxy.runtime.plist
    └── Resources/
        ├── AppIcon.icns
        ├── default-config.toml
        ├── THIRD-PARTY-NOTICES.md
        └── LICENSE
```

`Contents/MacOS/Siderostat` は現在の `siderostat-monitor` artifact を bundle 用に配置した名前で
あり、crate 名の即時 rename を要求しない。`Contents/Helpers/siderostat-runtime` は
現在の `siderostat` binary を配置した名前である。

Apple の bundle layout では helper tool を `Contents/Helpers` または `Contents/MacOS` に
置く。本仕様では main executable と明確に分けるため `Contents/Helpers` を使う。
また macOS 13 以降、LaunchAgent plist を app bundle の
`Contents/Library/LaunchAgents` に置き、実行 file を bundle 内に保つ構造を案内している。
この構造を採用し、`~/Library/LaunchAgents` へ新しい plist を copy しない。

### 5.2 identifier

`siderostat` は一般名詞でもあるため、製品の役割を含む
`dev.siderostat-ds4-proxy` を identifier root とする。先頭の `dev.` は開発者向けツールの
namespace を示すものであり、対応する Internet domain の取得または所有を前提としない。
GitHub などの hosting service、組織名、開発者・利用者の user name は含めない。

main app は root 自体を使い、同梱 component は root 配下の suffix で役割を示す。

| item | identifier |
|---|---|
| identifier root / main app | `dev.siderostat-ds4-proxy` |
| runtime code-signing id | `dev.siderostat-ds4-proxy.runtime` |
| LaunchAgent label | `dev.siderostat-ds4-proxy.runtime` |
| component pkg receipt | `dev.siderostat-ds4-proxy.pkg` |
| product archive | `dev.siderostat-ds4-proxy.product` |

公開済み bundle ID、LaunchAgent label、pkg receipt ID は互換性 identifier であり、通常の
release で変更しない。現在の `local.siderostat.runtime` と `local.siderostat.monitor` は
legacy identifier として migration logic にだけ残す。

### 5.3 `Info.plist`

最低限、次を持つ。

| key | value / policy |
|---|---|
| `CFBundleIdentifier` | `dev.siderostat-ds4-proxy` |
| `CFBundleExecutable` | `Siderostat` |
| `CFBundleName` | `Siderostat` |
| `CFBundleDisplayName` | `Siderostat` |
| `CFBundlePackageType` | `APPL` |
| `CFBundleShortVersionString` | semver release version |
| `CFBundleVersion` | 単調増加する build number |
| `LSUIElement` | `true` |
| `LSMinimumSystemVersion` | build parameter で固定し release note に記載 |
| `NSHighResolutionCapable` | `true` |

`LSUIElement = true` により main app は menu bar agent app として動作し、Dock に常時表示しない。
RDMA over Thunderbolt のためだけに app 全体の minimum OS を macOS 26.2 へ上げない。
base application の deployment target は実装開始時に別途確定し、RDMA profile は runtime
capability check で macOS 26.2 以上に限定する。

### 5.4 LaunchAgent plist

plist は署名済み app bundle の一部であり、install 時または first launch 時に書き換えない。
配布先は `/Applications/Siderostat.app` に固定されるため、pkg の postinstall から
`launchctl bootstrap` できるよう `Program` に固定 absolute path を指定する。
`BundleProgram` は Service Management 経由の登録には適しているが、pkg script からの直接
bootstrap では使用しない。

概念上の plist は次のとおりである。最終 key set は target macOS 上で `plutil` と
Service Management の実機試験を通す。

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.siderostat-ds4-proxy.runtime</string>

  <key>Program</key>
  <string>/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime</string>

  <key>ProgramArguments</key>
  <array>
    <string>siderostat-runtime</string>
    <string>serve</string>
  </array>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
</dict>
</plist>
```

署名済み plist に user home の絶対 path を埋め込めないため、runtime は `--config` 未指定時に
`~/Library/Application Support/siderostat/config.toml` を既定 path として解決しなければならない。
同様に `StandardOutPath` / `StandardErrorPath` を user ごとに plist へ書かず、runtime 自身の
structured file logging または Unified Logging で `~/Library/Logs/siderostat/` へ出力する。

## 6. Service Management と lifecycle

### 6.1 runtime の登録

main app は次と同等の Service Management 操作を行う。

```swift
let runtime = SMAppService.agent(
    plistName: "dev.siderostat-ds4-proxy.runtime.plist"
)
try runtime.register()
```

実装言語は Swift に限定しない。Rust から ServiceManagement framework を呼び出してよい。
ただし、`launchctl bootstrap` を main UI から shell command として呼ぶ方式は新構成では採用しない。

macOS 13 以降、`SMAppService` は app bundle 内の LoginItem、LaunchAgent、LaunchDaemon の
登録と制御を行い、`~/Library/LaunchAgents` へ plist を配置する旧方式を置き換える。
登録は user approval の対象であるため、main app は status を表示し、拒否または
`requiresApproval` の場合は目的を説明した上で System Settings の Login Items を開けるようにする。

### 6.2 monitor の login start

monitor 自身の「ログイン時に Siderostat を表示」は `SMAppService.mainApp` で管理する。
runtime の常駐許可と monitor の login start は別設定として表示する。

- runtime background service: 推論 service の稼働に必要
- monitor login start: menu bar UI を login 時に表示する利用者設定

初回 setup では両方を推奨値として `SMAppService` へ登録する。ただし利用者の承認状態を
偽装せず、Runtime と main app の各 `status` がともに `enabled` になるまで完了扱いにしない。
Runtime の開始／停止操作は Runtime の登録だけを変更し、main app の login start には影響させない。

### 6.3 menu 操作

新構成の menu semantic を次とする。

| 操作 | 挙動 |
|---|---|
| Siderostatを終了 | main app だけ終了。runtime は継続 |
| 設定ファイルを開く | `~/Library/Application Support/siderostat/config.toml` を既定アプリで開く。未作成時は最寄りの既存親フォルダを開く |
| siderostat-runtimeを再起動 | authenticated admin API で drain 後に self-restart。launchd が再起動 |
| siderostat-runtimeを停止して自動起動を無効化 | drain、`ds4-server` child stop、runtime service unregister。unregister が runtime process を終了 |
| siderostat-runtimeを起動して自動起動を有効化 | runtime service register。LaunchAgent は登録時に起動し、approval が必要なら案内 |

現行 monitor の `launchctl kickstart` / `bootout` 直接呼出しは互換 mode に限定し、新 bundle
mode では Service Management と admin API に置き換える。runtime が応答しない場合の recovery は、
明示確認の上で unregister/register を行う。既登録 service に対する shell command 依存を
通常経路にしない。

### 6.4 background activity の可視性

Siderostat のメニューバーアプリを終了しても runtime を継続する設計であるため、利用者が次を常に確認・停止できなければ
ならない。

- menu bar の `siderostat-runtime` 稼働状態
- System Settings > General > Login Items の Siderostat background item
- menu の「siderostat-runtimeを停止して自動起動を無効化」
- CPU、memory、model、cluster mode の概要

これは、background process が main app 終了後も動く場合に利用者へ可視性と停止手段を与える
Apple の guidance に従う。

## 7. data と executable の配置

### 7.1 user data

既存 deployment との互換性を優先し、初期版では lowercase の現行 path を維持する。

| data | path |
|---|---|
| config | `~/Library/Application Support/siderostat/config.toml` |
| secrets | `~/Library/Application Support/siderostat/secrets/` |
| manifests | `~/Library/Application Support/siderostat/manifests/` |
| runtime state | `~/Library/Application Support/siderostat/` |
| Siderostat cache | `~/Library/Caches/siderostat/` |
| DS4 KV cache | `~/Library/Caches/ds4-kv/` |
| logs | `~/Library/Logs/siderostat/` |

`.app`、`.pkg` の upgrade でこれらを削除または上書きしない。default config は bundle 内の
resource だが、既存 user config を暗黙に置換しない。schema migration は backup、validate、
atomic replace、rollback を備える。

### 7.2 DwarfStar と model

初期 `.pkg` は Siderostat の二 binary と resource だけを含む。

- GGUF は容量が大きく model 更新も独立しているため、絶対に app bundle へ含めない。
- `ds4` / `ds4-server` は現状どおり外部 executable を選択し、source commit と digest を
  manifest で承認する。
- app bundle の helper path から任意 executable を直接選ぶのではなく、runtime の既存
  canonical path、regular file、non-symlink、executable、digest 検証を維持する。

将来 DwarfStar binary を同梱する場合は、別の release policy と license review を行い、
`Contents/Helpers` へ同一 Team ID で署名した固定 binary として置く。user が差し替える
directory を署名済み app bundle 内に作らない。

## 8. code signing

### 8.1 certificate

| artifact | certificate |
|---|---|
| main app と nested executable | `Developer ID Application` |
| final installer package | `Developer ID Installer` |

development build は ad-hoc signing を許容するが、配布 artifact に ad-hoc signature を使わない。
すべての配布 executable で Hardened Runtime と secure timestamp を有効にする。
`com.apple.security.get-task-allow` を配布署名へ含めない。

### 8.2 signing order

inside-out で明示的に署名する。

1. `Contents/Helpers/siderostat-runtime`
2. その他の nested code があれば内側から順に署名
3. `Siderostat.app`
4. `.pkg`

nested helper は nonbundled executable なので、明示的な code-signing identifier
`dev.siderostat-ds4-proxy.runtime` を付ける。app と runtime は同じ Team ID で署名する。

署名時に `codesign --deep` を使わない。Apple は、nested code ごとに entitlement が異なること、
標準外 location の code を取りこぼし得ることから、`--deep` signing を避け、inside-out で
各 code item を署名するよう案内している。`--deep` は verification で必要な場合に限る。

### 8.3 entitlement policy

初期値は空または最小限とする。

- App Sandbox: 無効
- JIT: 無効
- unsigned executable memory: 無効
- disable library validation: 無効
- DYLD environment variable: 無効
- automation / Apple Events: 必要性がない限り無効

notification など entitlement や usage description が必要な機能は、monitor と runtime の
どちらが実行主体かを分けて付与する。library に entitlement を付けない。

## 9. `.pkg` 仕様

### 9.1 payload

final installer は flat product archive とし、payload は次の一項目だけを基準とする。

```text
/Applications/Siderostat.app
```

`/usr/local/bin`、`~/Library/LaunchAgents`、`/Library/LaunchDaemons` へ file を配置しない。
component package を `pkgbuild`、最終 product archive を `productbuild` で作成する。

概念上の build flow は次である。

```sh
ditto build/Siderostat.app build/payload-root/Siderostat.app

pkgbuild \
  --root build/payload-root \
  --component-plist build/component-plist.plist \
  --scripts build/pkg-scripts \
  --install-location /Applications \
  --identifier dev.siderostat-ds4-proxy.pkg \
  --version "$VERSION" \
  build/Siderostat-component.pkg

productbuild \
  --package build/Siderostat-component.pkg \
  --identifier dev.siderostat-ds4-proxy.product \
  --version "$VERSION" \
  --sign "Developer ID Installer: ..." \
  --timestamp \
  dist/Siderostat-"$VERSION".pkg
```

実際の `productbuild` option と Distribution XML の要否は build implementation で固定し、
同一 receipt ID と version 比較で upgrade 可能にする。component plist では
`BundleIsRelocatable=false` を指定し、既存 bundle identifier による `/Applications` 外への
relocation を許可しない。

### 9.2 installer script policy

`preinstall` と `postinstall` の二つの controlled script だけを許可する。
`preinstall` は次の順序で、bundle replacement 前に既存の製品プロセスを停止する。

1. `/dev/console` から現在の GUI ユーザーの UID を解決する。
2. `gui/<uid>/dev.siderostat-ds4-proxy.runtime` に限定して runtime LaunchAgent を
   `launchctl bootout` する。job が未登録・未稼働の場合は安全な no-op とする。停止に失敗した
   場合は同じ job target への `launchctl kill SIGKILL` と、bundle 内の
   `/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime` という完全一致した
   実行パスへの fallback を使う。
3. runtime job と完全一致した runtime executable が消えるまで待機する。
4. `/Applications/Siderostat.app/Contents/MacOS/Siderostat` という完全一致した実行パスの
   Monitor に SIGTERM を送り、最大10秒待機する。終了しない場合だけ同じ完全一致の Monitor
   に SIGKILL を送り、他のプロセスへ波及させない。

runtime の停止を先に行うことで、旧 Monitor が停止中の runtime を検出して再操作する競合を
避ける。`preinstall` は Service Management の登録状態、ユーザーの承認、plist ファイル、
設定、secret、model、cache を変更・削除しない。`bootout` は bundle replacement のための
一時的な job unload であり、インストール後に起動した新しい Siderostat が既存の登録・承認状態を
読み取り、必要な runtime を復帰させる。

`postinstall` は、`launchctl print-disabled gui/<uid>` で product-owned runtime が事前に
`enabled` だった場合だけ、新しい bundle 内の LaunchAgent plist を同じ `gui/<uid>` domain へ
bootstrap し、その後アクティブな console user の GUI session で `/Applications/Siderostat.app`
を起動する。disabled、未登録、承認待ちの場合は bootstrap せず、アプリ側の Service Management
承認導線へ委譲する。これにより upgrade 前に稼働していた runtime は、ユーザーの停止設定を変更
せずにインストール直後から復帰する。

installer process から次を行わない。

- console user 以外のユーザー・session の操作
- `launchctl disable`、`SMAppService.unregister()`。`launchctl bootstrap gui/<uid>` は、
  事前状態が `enabled` の product-owned runtime を復帰させる場合に限り許可する。
- `~/Library/LaunchAgents` の変更
- user config / secret の生成または上書き
- runtime job 以外のプロセス、任意の `ds4-server` child、未知 PID の停止または強制終了
- Recovery での RDMA 有効化

runtime と main app login start の登録は、install 後に postinstall が起動した Siderostat の
user session で `SMAppService` を通して行う。approval が必要な場合は app が Login Items の
System Settings を開く。console user がいない場合、postinstall は install を失敗させず起動を
スキップする。両 script は idempotent、rollback-safe、user data preserving とする。

### 9.3 installer UX

- install 先は `/Applications`
- package installation の管理者承認と、runtime background item の user approval は別物
- install 完了後、Siderostat を自動起動して background service と Siderostat の login start を
  登録する。approval が必要な場合は Siderostat が System Settings > General > Login Items を開く
  （許可状態はユーザーが管理する macOS の状態であり、package の管理者承認とは別物）
- model は同梱されず、既存設定を検出するか setup で選択する旨を明示

## 10. notarization

配布 `.pkg` は次の順で作る。

1. 全 executable を Developer ID Application、Hardened Runtime、timestamp 付きで署名
2. app signature を検証
3. Developer ID Installer で final `.pkg` を署名
4. `xcrun notarytool submit --wait` で final `.pkg` を Apple notary service へ送信
5. notary log を取得し warning を確認
6. `xcrun stapler staple` で ticket を `.pkg` に付与
7. `xcrun stapler validate` と Gatekeeper 検証

Apple は Developer ID 配布に、全 executable の有効な署名、Hardened Runtime、secure
timestamp、適切な Developer ID certificate を要求している。flat installer package は
notarization 対象であり、旧 `altool` ではなく `notarytool` を使う。

## 11. first launch

初回起動は次の順序で行う。

1. app version、signature/build metadata を表示可能にする。
2. legacy install を検出する。
3. user config、secret、manifest を読み取り、schema と permission を検証する。
4. runtime LaunchAgent と main app login item の status を取得する。
5. background service と monitor login start の目的を説明し、未登録のものを登録する。
6. approval が不足する場合は System Settings を開く導線を示す。
7. runtime と monitor login start の両 status が `enabled` になったことを確認する。
8. runtime admin API readiness を待つ。
9. DwarfStar/model manifest readiness を表示する。

初回起動時に model を load して数分 UI を block しない。runtime registration と model
startup を別の progress state として表示する。

## 12. legacy migration

### 12.1 検出対象

- `/usr/local/bin/siderostat`
- `/usr/local/bin/siderostat-monitor`
- `~/Library/LaunchAgents/local.siderostat.runtime.plist`
- `~/Library/LaunchAgents/local.siderostat.monitor.plist`
- `gui/<uid>/local.siderostat.runtime`
- `gui/<uid>/local.siderostat.monitor`

### 12.2 migration 手順

1. legacy job と PID、実行 path を identity verification する。
2. 利用者へ重複起動を避ける migration を説明する。
3. legacy runtime を drain し、legacy monitor/runtime job を user domain から停止する。
4. legacy plist を削除せず、Application Support 内の migration backup へ退避する。
5. new runtime LaunchAgent を `SMAppService` で登録する。
6. new runtime readiness と config compatibility を確認する。
7. 成功後、legacy binary は「削除可能」と表示するが、自動削除しない。

new service の登録または readiness に失敗した場合は、legacy plist を戻して以前の job を
再開できる rollback path を持つ。old/new runtime を同時起動して同じ HTTP port、state、DS4
child を競合させない。

`SMAppService.statusForLegacyPlist(at:)` を利用できる target では、legacy authorization
状態の判定にも使う。

## 13. upgrade と rollback

### 13.1 upgrade

- `.pkg` は `/Applications/Siderostat.app` を新しい署名済み bundle で更新する。
- user data、model、cache、secret を保持する。
- LaunchAgent plist の label と bundle-relative helper path を安定させる。
- active runtime は旧 executable image のまま動き得るため、monitor は app version と
  runtime version を比較し、不一致を macOS 通知で知らせる。runtime の再起動は
  ユーザーが既存のメニュー項目から明示的に実行する。
- plist schema または identifier を変える release は、明示的 unregister/register migration を持つ。

### 13.2 rollback

- prior notarized `.pkg` を保持する。
- config migration は backward compatibility または backup restore を提供する。
- runtime binary と config schema が非互換なら、旧 package install 前に rollback warning を出す。
- model、manifest、secret、KV cache を rollback 時に削除しない。

## 14. uninstall

`.pkg` には uninstall UI を埋め込まず、リリース DMG に同梱する
`Siderostat Uninstaller.app` を製品の標準 uninstall 導線とする。

Uninstaller は、エンドユーザーに Terminal 操作を要求せず、確認ダイアログの後に次を順序どおり
実行する。

1. `SMAppService` から runtime と Siderostat のログイン項目を unregister する。
2. runtime、管理対象の `ds4-server` child、Monitor が停止したことを確認する。
3. `/Applications/Siderostat.app` だけを Trash へ移す。
4. package receipt を対象 identifier に限定して整理する。
5. 処理結果を画面に表示する。

Trash 移動と package receipt の整理は、一つの `osascript` 管理者承認トランザクション内で
実行する。これにより Finder の移動操作と receipt 整理で管理者パスワードを二重に要求しない。
Trash 内 bundle の所有者を後から `chown` で変更しない。macOS が Trash に付与する保護属性と
衝突するためであり、Trash の所有ユーザーによる通常の管理に委ねる。既存の同名 bundle がある
場合は `.1` 以降の一意な退避名を選び、既存項目を上書きしない。
Uninstaller は `SMAppService.unregister()` で登録を解除するが、macOS が保持する Login Items /
Background Items の承認履歴を強制的に消去しない。再インストール時に同じユーザーの承認を再利用
できることを優先し、履歴を消去したい場合はユーザーが System Settings から明示的に変更する。

既定では Application Support、secret、model、cache を残す。「すべてのデータを削除」は、
対象 path を列挙し、再確認し、app bundle と Siderostat 管理下 data 以外を削除しない専用操作とする。

Uninstaller は次の安全契約を満たす。

- 初回実行済み・未実行・一部停止済みのいずれでも安全に再実行できる。
- 未確認のプロセス、他のアプリ、他の LaunchAgent を停止しない。
- `~/Library/Application Support/siderostat`、secret、manifest、cluster state、model、
  `~/Library/Caches/ds4-kv` を既定操作で削除しない。
- `sudo rm -rf` などの不可逆な一括削除を使用しない。
- `uninstall-siderostat.sh` は CI・実機検証用の補助 CLI として扱い、エンドユーザー向けの正式な
  導線にはしない。

## 15. build artifact

release job は少なくとも次を出力する。

- `Siderostat.app` を格納した notarized distribution archive
- `Siderostat-<version>.pkg`
- `Siderostat-<version>.dmg`（上記 `.pkg`、`Siderostat Uninstaller.app`、README を収録）
- `Siderostat Uninstaller.app`（Developer ID Application 署名・公証済み）
- SHA-256 checksum
- build metadata（git commit、Rust version、target、build number）
- SBOM または dependency inventory
- third-party notices
- notarization submission ID と log

local development では、同じ bundle layout を ad-hoc signing で作る `app-dev` target と、
certificate と notary credential を必要とする `dist-pkg` target を分ける。credential を repository、
artifact、log に出力しない。

## 16. verification と acceptance criteria

### 16.1 static verification

- `plutil -lint` が `Info.plist` と LaunchAgent plist に成功する
- `codesign --verify --deep --strict --verbose=4 Siderostat.app` が成功する
- helper と app の Team ID、identifier、Hardened Runtime が期待値と一致する
- `spctl --assess --type execute` が app を受理する
- `pkgutil --check-signature` が package を受理する
- `spctl --assess --type install` が package を受理する
- `xcrun stapler validate` が package に成功する
- DMG に想定した `.pkg`、`Siderostat Uninstaller.app`、README だけが含まれる
- DMG の Developer ID 署名と `spctl --assess --type open --context context:primary-signature`
  が成功する
- `Siderostat Uninstaller.app` の署名、公証、Gatekeeper 検証が成功する
- Uninstaller.app が独自の仮アイコンを同梱せず、macOS の標準アプリ表示になる
- Uninstaller の app 移動と receipt 整理が一回の管理者承認で完了する
- final checksum が release metadata と一致する

### 16.2 clean install

- clean Mac へ `.pkg` を install できる
- `/Applications` 以外に system payload を作らない
- package install 完了後に Siderostat が自動起動し、first launch で background item と Siderostat の
  login item の登録、説明、approval flow が動く
- runtime と monitor が次回 login で設定どおり起動する
- monitor を終了しても runtime が継続する
- runtime crash 後に launchd が再起動する
- background service を停止すると再起動しない

### 16.3 upgrade / migration

- legacy LaunchAgent から new bundle service へ重複なしで移行できる
- existing config、secret、manifest、model、KV cache を保持する
- active runtime を含む upgrade 後、version mismatch を解消できる
- prior package と config backup で rollback できる
- failed migration で legacy service を復旧できる

### 16.4 security

- runtime は logged-in user 権限で動き、root process を作らない
- secret directory `0700`、secret file `0600` を維持する
- admin API は loopback と token authentication の現行方針を維持する
- runtime は manifest-approved DwarfStar executable/model だけを起動する
- app bundle 内の code または plist を install 後に変更しない
- Gatekeeper offline test でも stapled ticket を確認できる

### 16.5 uninstall

- DMG から `Siderostat Uninstaller.app` を起動して確認ダイアログを表示できる
- runtime と Siderostat のログイン項目が unregister される
- runtime、管理対象 `ds4-server` child、Monitor、app bundle が停止・削除される
- package receipt が対象 identifier に限定して整理される
- Application Support、secret、manifest、cluster state、model、KV cache が保持される
- Uninstaller を再実行してもエラーやデータ削除が発生しない

## 17. 実装 milestone

### Phase 1: unsigned/ad-hoc `.app`

- deterministic bundle builder
- `Info.plist`、icon、resource
- monitor main executable と runtime helper の配置
- runtime default config path
- bundle 内 LaunchAgent plist
- local `SMAppService` register/status/unregister

### Phase 2: lifecycle と migration

- menu 操作を admin API / Service Management へ移行
- `SMAppService.mainApp` login start
- legacy detection、backup、rollback
- runtime/app version handshake

### Phase 3: signed `.pkg`

- Developer ID Application signing
- package build と Developer ID Installer signing
- `notarytool`、stapling、verification
- clean machine acceptance

### Phase 4: release hardening

- upgrade/rollback matrix
- failure injection
- reproducible metadata、checksum、SBOM
- user-facing installation、background service、uninstall documentation

### Phase 5: distribution uninstall UX

- release DMG に `.pkg` と `Siderostat Uninstaller.app` を同梱
- Uninstaller の確認 UI、Service Management unregister、app bundle の Trash 移動
- DMG、Uninstaller.app の署名、公証、Gatekeeper 検証
- user data 保持と idempotent uninstall の実機受入

## 18. 保留事項

次は本仕様の構造を変えず、実装開始前または初回公開前に確定できる。

- base application の最低 macOS version
- Apple Developer Team
- icon と表示名
- DwarfStar binary を将来同梱するか
- auto-update の方式
- runtime restart 用 admin endpoint の詳細
- Unified Logging と file logging の最終方針

## 19. Apple 公式資料

- [`SMAppService`](https://developer.apple.com/documentation/servicemanagement/smappservice)
- [`SMAppService.register()`](https://developer.apple.com/documentation/servicemanagement/smappservice/register%28%29)
- [`SMAppService.unregister()`](https://developer.apple.com/documentation/servicemanagement/smappservice/unregister%28%29)
- [`SMAppService.Status`](https://developer.apple.com/documentation/servicemanagement/smappservice/status-swift.enum)
- [Updating helper executables from earlier versions of macOS](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos)
- [Updating your app package installer to use the new Service Management API](https://developer.apple.com/documentation/servicemanagement/updating-your-app-package-installer-to-use-the-new-service-management-api)
- [Managing ongoing background processes in your Mac](https://developer.apple.com/documentation/appkit/managing-ongoing-background-processes-in-your-mac)
- [`LSUIElement`](https://developer.apple.com/documentation/bundleresources/information-property-list/lsuielement)
- [Placing content in a bundle](https://developer.apple.com/documentation/bundleresources/placing-content-in-a-bundle)
- [Creating distribution-signed code for macOS](https://developer.apple.com/documentation/xcode/creating-distribution-signed-code-for-the-mac)
- [Packaging Mac software for distribution](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution)
- [Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime)
- [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [TN3205: Low-latency communication with RDMA over Thunderbolt](https://developer.apple.com/documentation/technotes/tn3205-low-latency-communication-with-rdma-over-thunderbolt)
