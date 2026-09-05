# siderostat xtask

`cargo xtask <command>` でインストール・検証・アンインストールを自動化する。

## v0.3.1 の提供方針

v0.3.1 の公式提供物はソースコードであり、事前ビルド済みの `.app`、`.pkg`、DMG、
`Siderostat Uninstaller.app` は配布しない。利用者向けの導入経路は、ソース checkout から
`cargo xtask install --start` を実行する方法である。

`app-dev`、`pkg-dev`、`dmg-dev`、`sign` は、各 Mac 上での bundle/package 構造確認、
署名・公証の切り分け、または将来の任意バイナリ配布に備えた開発者向け workflow である。
これらの artifact と Apple Developer ID、notary profile、secure timestamp は、v0.3.1 の
ソースリリース受入条件ではない。

```sh
cargo xtask install [options]
cargo xtask fingerprint-models [options]
cargo xtask verify
cargo xtask uninstall
cargo xtask app-dev [options]
cargo xtask pkg-dev [options]
cargo xtask dmg-dev [options]
cargo xtask sign [options]
```

`cargo xtask` は `.cargo/config.toml` の alias で `cargo run --package xtask --` に解決される。

## install

Obsidian Vault の `Projects/siderostat/docs/internal-installation.md` 第5節と `contrib/launchd/README.md` の手順を1コマンドで実行する。

実行順:

1. （`--ci` 指定時のみ）Required CI gate: `cargo fmt --check` / `clippy -D warnings` / `test` / `git diff --check`。
2. `~/` 配下から `ds4-server` と対象GGUFを探し、親ディレクトリを `DWARFSTAR_HOME` とする。
3. GGUFのSHA-256計算を行うか確認する。既定は **行わない**。実行する場合はその場で計算し、行わない場合は事前に保存したdigest cacheだけを使う。
4. `cargo build --release` で runtime と monitor をbuildする。
5. `codesign --force --sign -` で `target/release/siderostat` と
   `target/release/siderostat-monitor` を再署名（launchd の launch constraint を満たすため）。
6. `sudo install` で `/usr/local/bin/siderostat` と `/usr/local/bin/siderostat-monitor` へコピーし、両方の署名を再検証。
7. secret を生成（`~/Library/Application Support/siderostat/secrets/`、mode 0600、32+ bytes）。
   既存は上書きしない。2-node cluster では `--shared-secret-dir` で共有 secret を供給する。
8. `siderostat.example.toml` から config を生成し、**全 placeholder**（binary / model / DSpark support / manifest / secret / node_id）を実在 path へ置換。
9. standalone/distributed manifest を生成。argv profile は siderostat 本体の argv builder を再利用して算出する。
10. runtime と monitor の LaunchAgent plist を `~/Library/LaunchAgents/` へ install（`USERNAME` を現在ユーザーへ置換、config/log path を設定、`plutil -lint`、placeholder guard）。
   既定では **起動しない**（`--start` 指定時のみ bootstrap + kickstart。起動は ds4-server を再起動するため）。

### GGUF digest

モデルをダウンロードまたは置換した後、次のコマンドを一度実行する。モデルの内容を読み、`~/Library/Application Support/siderostat/manifests/digest-cache.json`へSHA-256とファイルメタデータを保存する。manifestやLaunchAgentは変更しない。

```sh
cargo xtask fingerprint-models [options]
```

以後の`cargo xtask install`では、確認プロンプトに`N`（既定）を選ぶとこのcacheを使用し、GGUF全体を再読込しない。metadataが完全一致する場合は即時再利用する。inodeやmtimeだけが変わりsizeが同じ場合は、最大4 MiBの分散サンプル署名を確認し、一致すればcache metadataを自動更新する。size変更またはサンプル不一致ではfull SHA-256の再計算を要求する。

旧形式cacheにはサンプル署名がないため、metadata driftを内容変更とは断定せず停止する。ファイルが移動・複製・`touch`されただけで内容が不変だとoperatorが確認済みの場合は、`--accept-model-metadata-change`を一度指定すると、cached full digestを維持したままmetadataとサンプル署名を更新できる。不明な場合は`cargo xtask fingerprint-models`または`--hash-models`でfull SHA-256を再計算する。サンプル署名は偶発的な変更を高速検出するためのもので、full-file SHA-256の代替ではない。

### install options

| option | 意味 |
|---|---|
| `--ds4-server <path>` | ds4-server の場所を明示（既定: `$HOME` 探索） |
| `--node-id <name>` | config に書く `cluster.node_id`（既定: hostname） |
| `--standalone-model` / `--distributed-model` / `--dspark-support` | モデルを明示（既定: gguf 配下を名前で自動判別）。`--mxfp4-model` は旧 alias |
| `--shared-secret-dir <dir>` | 共有する cluster-control / peer-proxy の供給元（legacy `.key` も可） |
| `--ds4-source-commit <sha>` | distributed manifest の verified DS4 commit（初回 install で必須） |
| `--ds4-binary-digest <sha>...` | distributed manifest の承認済み binary digest 集合（既定: 実機 digest） |
| `--peer-ds4-binary-digest <sha>...` | 相手nodeの binary digest。自nodeのdigestはinstall時に計算して集合へ自動追加 |
| `--hash-models` | GGUFのSHA-256を確認せず計算・更新する（既定はプロンプト、既定選択は実行しない） |
| `--accept-model-metadata-change` | size不変のmetadata driftをoperator確認済みとして受理し、旧cacheを再ハッシュなしで更新する |
| `--ci` | インストール前に Required CI gate を実行 |
| `--start` | LaunchAgent を bootstrap + kickstart する（ds4-server を再起動） |

2-nodeでnative最適化の異なるds4-serverを使う場合は、各nodeで相手側のdigestだけを指定する。
installは自nodeのdigestを計算し、相手側のdigestと合わせて同じ
`compatible_ds4_binary_sha256`集合を両nodeのmanifestへ書き出す。

```sh
# coordinator: workerのdigestを指定
cargo xtask install --peer-ds4-binary-digest "<worker-ds4-sha256>"

# worker: coordinatorのdigestを指定
cargo xtask install --peer-ds4-binary-digest "<coordinator-ds4-sha256>"
```

`--ds4-binary-digest`は承認済み集合全体を明示する上級者向け指定として残している。
両オプションは同時に指定できない。

### 手順認識の訂正（ユーザーの当初理解に対して）

- **署名は正しい**（必要）。launchd の launch constraint が linker-signed adhoc を拒否するため、`codesign` での再署名が必須。`/usr/local/bin` へのコピーは署名を保持する。
- **plist の USERNAME 置換だけでなく**、config path・log path を明示設定し、`plutil -lint` と placeholder guard を行う。
- **config は `ds4.binary` だけでなく全 placeholder を置換**する必要がある（model / DSpark support / manifest / secret / node_id）。
- **manifest はモデルの sha256 だけでは作れない**。`argv_profile_sha256`・`ds4_binary_sha256`・DSpark binding・layer/wire schema 等を含むため、本体の argv builder と manifest schema を再利用する。
- **secret は cluster-control と peer-proxy が両 node で共有**される。単一 node の自動生成では共有されないため、2-node 運用では `--shared-secret-dir` を使う。
- **他に必要な手順**: Required CI、config の schema/validation、署名後の verify、LaunchAgent の lint/guard、admin API 検証。

## verify

`launchctl print` と admin API（`/healthz` `/readyz` `/cluster` `/metrics`）の到達性を確認する。siderostat が起動していない場合は unreachable を表示する。

## uninstall

`launchctl bootout` して plist を `.disabled` へ退避する。model / KV cache / secret / runtime state は削除しない。

## app-dev

`contrib/macos/` の bundle template と release build の runtime / monitor binary から、
production と同じ layout の `Siderostat.app` を ad-hoc 署名で生成する（B-03）。指定した
version/build number は Info.plist と runtime の `/healthz` metadata の両方へ注入される。

```sh
cargo xtask app-dev --version <semver> --build-number <integer> [--verify]
```

- staging は既定で `build/app-dev/`。毎回空の状態から再構築するため、同一入力なら
  file 一覧・plist 値・unsigned content digest が一致する。
- `Contents/MacOS/Siderostat` は monitor、`Contents/Helpers/siderostat-runtime` は runtime。
- bundle 内 LaunchAgent plist は固定インストール先 `/Applications/Siderostat.app` の
  `Program` absolute path を使い、user home の絶対 pathを書かない。pkg の postinstall から
  直接 bootstrap するためである。
- helper → main app の順に ad-hoc 署名する。signing に `--deep` は使わない。
- `AppIcon.icns` は既定で placeholder を自動生成する。`--icon <path>` で正式 icon を指定できる。
- `--verify` 指定時は `plutil -lint`、`codesign --verify --deep --strict --verbose=4` を実行する。
- user data と `/Applications` は変更しない。certificate / notary は不要。

## pkg-dev

`app-dev` で生成した bundle を flat `.pkg` にする（E-01）。インストール前に、active console
user の `gui/<uid>/dev.siderostat-ds4-proxy.runtime` だけを LaunchAgent から unload し、
その後 `/Applications/Siderostat.app/Contents/MacOS/Siderostat` の完全一致した Monitor を終了する
`preinstall` script を含める。runtime job または bundle 内の完全一致した runtime executable が
残る場合、Monitor が通常終了しない場合だけ対象を限定して強制終了する。インストール完了後は
アクティブなコンソールユーザーの GUI セッションで `/Applications/Siderostat.app` を起動する
`postinstall` script を実行する。任意の process、`ds4-server`、ユーザーデータは対象にしない。

```sh
cargo xtask pkg-dev --app-dir <staging> --version <semver> [--output-dir dist]
```

payload は `/Applications/Siderostat.app` 一項目のみ。`postinstall` は、更新前に runtime が
`enabled` だった場合に限り新しい bundle 内の LaunchAgent を同じ GUI domain へ bootstrap し、
その後アプリを起動する。disabled、未登録、承認待ちの場合は bootstrap せず、ログイン項目や
Service Management の承認・恒久登録、設定・secret の変更はアプリ本体へ委ねる。
GUI ユーザーが存在しない場合は、インストールを失敗させずに起動をスキップする。

通常の package は既存 app の bundle version を検査する。明示的な旧版復元が必要な場合だけ
`--rollback` を指定する。この場合は `BundleIsVersionChecked=false` の rollback package を生成し、
成果物名は `Siderostat-<version>-rollback.pkg` になる。runtime、LaunchAgent、設定、secret、model、
cache には触れない。

```sh
cargo xtask pkg-dev \
  --app-dir <staging> \
  --version <semver> \
  --rollback \
  --output-dir dist
```

## macOS CI の ad-hoc artifact verification

証明書、Keychain、notarization credential、network を使わず、macOS CI で bundle/package の構造を検査する。
`app-dev` と `pkg-dev` を実行した後、plist、layout、nested signature、package payload、installer script、
禁止 path、secret・user data・model の混入を確認する。壊れた plist、余分な payload、unsigned helper の
fixture も個別に失敗することを確認する。

```sh
bash scripts/verify-macos-dev-artifacts.sh
```

## sign

Developer ID Application / Installer 署名、公証、stapling、最終検証を一つの順序で実行する。
証明書の private key や notarization credential 本文は受け取らず、Keychain の identity 名と
`notarytool` の Keychain profile 名だけを受け取る。

```sh
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.1 \
  --build-number 7 \
  --application-identity "Developer ID Application: Example (TEAMID)" \
  --installer-identity "Developer ID Installer: Example (TEAMID)" \
  --notary-profile siderostat-notary \
  --output-dir dist
```

タイムスタンプ障害を切り分ける必要がある場合だけ、`--timestamp-mode none` を明示的に指定できる。
このモードは `codesign` と `productbuild` に `--timestamp=none` を渡し、公証・staple・Gatekeeper
検証を実行しない。成果物には `-no-timestamp` が付与され、metadata には
`timestamp_mode=none` と `distribution_ready=false` が記録されるため、通常の配布 artifact を上書きしない。
公証 profile も不要だが、これはローカル診断専用であり、エンドユーザー配布には使用しない。

```sh
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.1 \
  --build-number 14 \
  --application-identity "Developer ID Application: Example (TEAMID)" \
  --installer-identity "Developer ID Installer: Example (TEAMID)" \
  --timestamp-mode none \
  --output-dir dist/no-timestamp-build14
```

実行前は `--dry-run` を付ける。dry-run は helper → app → package → notary submit/wait →
log → staple → validate → Gatekeeper の順序、固定 identifier、出力 path を表示するが、
Keychain profile の実値、Keychain、ネットワーク、artifact は使用・変更しない。
`--timestamp-mode none` の dry-run では、notary submit、staple、Gatekeeper 検証がスキップされることと、
`--timestamp=none` および `-no-timestamp` の出力名を確認できる。
notary log は既定で `dist/notary/`、build metadata は `dist/Siderostat-<version>.metadata.json`
へ保存される。metadata に credential、profile 名、private key は含めない。

旧版を明示的に配布する場合は `--rollback` を追加する。成果物、notary log、metadata に
`-rollback` が付与され、metadata の `rollback` と `install_mode` にも記録される。通常 package の
生成経路は変更されず、誤った downgrade は発生しない。

```sh
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.1 \
  --build-number 10 \
  --rollback \
  --application-identity "Developer ID Application: Example (TEAMID)" \
  --installer-identity "Developer ID Installer: Example (TEAMID)" \
  --notary-profile siderostat-notary \
  --output-dir dist/rollback-build10
```

### 任意のローカル DMG と Uninstaller.app 検証

公式のエンドユーザー配布物ではないローカル検証用 DMG を作成する場合は、`.pkg`、
`Siderostat Uninstaller.app`、`README.html` だけを含める。これは bundle/package の構造、
Uninstaller の挙動、または将来の任意バイナリ配布仕様を確認するためのものであり、
v0.3.1 のリリース artifact にはならない。

```sh
cargo xtask dmg-dev \
  --app-dir build/app-dev \
  --package dist/Siderostat-0.3.1.pkg \
  --version 0.3.1 \
  --build-number 11 \
  --output-dir dist \
  --verify
```

将来、公式バイナリ配布を別リリースで採用する場合に Developer ID 署名、公証、staple、Gatekeeper確認まで
行うときは、`sign` に `--with-dmg` を追加する。
既存の app/pkg の署名・公証に続けて Uninstaller を署名し、DMG を公証して `dist/notary/` と metadata に
結果を保存する。Uninstaller は notarytool が受け付ける一時 zip として提出し、DMG は Developer ID
Application 署名後に提出する。`Siderostat Uninstaller.app` は `.pkg` の payload や `/Applications` 配下には配置しない。

```sh
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.1 \
  --build-number 11 \
  --application-identity "Developer ID Application: Example (TEAMID)" \
  --installer-identity "Developer ID Installer: Example (TEAMID)" \
  --notary-profile siderostat-notary \
  --output-dir dist \
  --with-dmg
```

Uninstaller の標準動作は、Service Management の解除、対象 process の停止、`Siderostat.app` の Trash 移動、
正確な package receipt の整理である。Application Support、secret、manifest、cluster state、model、KV cacheは
保持する。v0.3.1 のソース導入では `cargo xtask uninstall` を使用する。Uninstaller.app を含む DMG の導線は、
将来の任意バイナリ配布を採用した場合に別途定義する。
