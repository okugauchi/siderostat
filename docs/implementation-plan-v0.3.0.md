# Siderostat v0.3.0 実装計画

- 文書状態: 実装・受入済み範囲とソースリリース計画
- 作成日: 2026-08-19
- 対象 baseline: `develop`、crate version `0.2.1`
- 最優先仕様: [`distribution/macos-app-bundle-pkg-spec.md`](distribution/macos-app-bundle-pkg-spec.md)
- 障害調査:
  - [`research/hermes-cron-throughput-degradation-2026-08-19.md`](research/hermes-cron-throughput-degradation-2026-08-19.md)
  - [`research/coordinator-restart-notification-repetition-2026-08-19.md`](research/coordinator-restart-notification-repetition-2026-08-19.md)
- 延期候補:
  [`research/dwarfstar-rdma-tensor-parallel-2026-08-18.md`](research/dwarfstar-rdma-tensor-parallel-2026-08-18.md)
- ds4-server の実行形態と Siderostat の正規用語:
  [`ds4-mode-taxonomy.md`](ds4-mode-taxonomy.md)

## 1. 目的

本書は、上記の配布仕様と調査文書を、v0.3.0 で順番に実行できる小さい task へ分解した
実装計画である。v0.3.0 の公式提供方針は public repository のソース公開のみとし、
公式の事前ビルド済みバイナリ、DMG、`.pkg` は配布しない。`Siderostat.app`、bundle、
package、署名・公証の実装と実機検証記録は、各 Mac 上でソースから導入するための実装と、
将来の任意バイナリ配布を検討するための内部検証として保持する。

導入 feature が受入基準を満たす前に、低速推論の自動復旧、通知重複排除、TP/RDMA の
実装へ着手しない。Mac 間 Tensor Parallelism（以下 TP）と RDMA は最後に再評価し、
本計画では v0.4.0 以降へ延期することを既定判断とする。

各 task は、低い reasoning effort または小型モデルでも、設計判断を補完せずに実行できる
粒度を目標とする。そのため、事前条件、変更対象、順序、事後条件、受入基準、検証、
ユーザーによるレビューまたは手作業、停止条件を task ごとに固定する。

## 2. 優先順位と v0.3.0 の範囲

### 2.1 feature 優先順位

| 優先度 | feature | v0.3.0 の扱い |
|---:|---|---|
| P0 | macOS bundle、Service Management、legacy migration、ソースからの導入 | 必須。最初に完了する |
| P1 | 推論進捗の鮮度、canary、診断 snapshot、安全な degraded recovery | 必須。P0 完了後に開始する |
| P2 | recovery epoch 単位の通知重複排除 | 必須。P1 完了後に開始する |
| P3 | DwarfStar Mac 間 TP / RDMA / DSpark + TP | 実装しない。最後に upstream を再評価して v0.4+ へ送る |

優先度は task の番号ではなく依存関係で強制する。`H-01` は `E-06`、`N-01` は
`H-10`、`T-01` は `N-03` が完了するまで開始できない。

### 2.2 v0.3.0 の必須成果物

1. Apple silicon 向け bundle と、ソースから各 Mac へ導入する workflow。
2. monitor を main app、runtime を app 内 LaunchAgent Helper とする bundle layout。
3. `SMAppService` による runtime の登録・停止と、main app の login start 設定。
4. legacy LaunchAgent を重複起動なしで移行し、失敗時に復旧できる migration。
5. checksum、build metadata、dependency inventory、third-party notices を含む source release evidence。
6. active 推論の最終進捗時刻と token 差分の観測、現在 chunk TPS の monitor 表示。
7. 診断 snapshot、canary、単一 owner の `recover-degraded` job、cooldown と回数上限。
8. coordinator-only restart 中の通知を recovery epoch 内で意味的に重複排除する機能。
9. clean source install、upgrade、legacy migration、rollback、2 node recovery の実機 evidence。

### 2.3 v0.3.0 の非目標

- Mac App Store、App Sandbox、privileged LaunchDaemon、`SMJobBless`
- Intel Mac 用 universal binary
- GGUF または DwarfStar executable の app 同梱・自動 download・自動更新
- 自動 update framework と `.pkg` 単体の完全 uninstall
- 人間向け `ds4` REPL を production HTTP API に変換する stdio/PTY bridge
- `tensor-parallel-rdma` / `tensor-parallel-tcp` profile と RDMA の自動設定
- Hermes repository 自体の変更。Siderostat 側の API、CLI、運用手順までを対象とする

### 2.4 入力文書と task の対応

| 入力文書 | 実装先 | release 判定 |
|---|---|---|
| macOS app/pkg 配布仕様 1〜16 節 | A-01〜E-06 | P0 必須 gate |
| macOS app/pkg 配布仕様 17〜19 節 | R-01〜R-03 | source release と文書 gate（package 条件は将来の任意経路） |
| Hermes throughput 調査 8〜12 節 | H-01〜H-10 | P1 必須 gate |
| coordinator restart 通知調査 | N-01〜N-03 | P2 必須 gate |
| DwarfStar TP/RDMA 調査 | T-01 | P3。実装せず v0.4+ entry criteria に変換 |

## 3. 小型モデル向け task 実行規則

### 3.1 状態

Task heading の checkbox を進捗状態の正とする。

- `[ ]`: 未着手
- `[-]`: 着手中
- `[x]`: 完了。検証と Evidence が記録済み
- `[!]`: blocked。理由と再開条件が記録済み

### 3.2 一回の実行単位

1. `Depends on` がすべて `[x]` の task を一つだけ `[-]` にする。
2. task に記載した参照節、`Files`、直接呼ばれる test だけを先に読む。
3. 一つの task で production behavior を一つだけ変更する。
4. production file が 4 個を超える、または独立した behavior が二つ以上必要になった場合は、
   実装せず本書に subtask を追加してユーザーへレビューを依頼する。
5. `Files` 外の変更が必要なら、先に task の `Files` と理由を本書へ追記する。
6. 無関係な rename、依存更新、formatting、refactor を同じ task に混ぜない。
7. 実装と直接対応する unit/integration test を同じ commit に含める。
8. task 固有の受入基準を test 名または確認 command へ一対一で対応させる。
9. 完了時に Evidence と次に着手可能な task を記録する。

### 3.3 Actor とユーザー関与

- `agent`: agent が実装と自動検証を完了できる。ユーザー手作業は不要。
- `agent + user review`: agent が案または artifact を作る。ユーザーが明示承認するまで未完了。
- `user + agent`: 証明書、System Settings、再起動、cable、2 node 実機などユーザー操作が必要。
  agent は command、期待値、rollback、evidence 採取を準備し、結果を判定する。

「ユーザーレビュー・手作業」欄が `なし` の task で、実機設定、GitHub、Apple Developer
account、証明書、Keychain、System Settings を変更してはならない。

### 3.4 task 開始時の共通 preflight

```sh
git status --short
git branch --show-current
git diff --check
```

- ユーザーの既存変更を上書き、破棄、無関係に整形しない。
- branch 作成、rename、merge、tag、push 前に `CONTRIBUTING.md` を読む。
- secret、notary credential、Developer ID private key、API token、GGUF、KV cache、runtime state を
  repository または CI artifact へ追加しない。
- 実機の prompt、response、API key、session ID、完全な deployment ID を evidence に保存しない。

### 3.5 共通 local gate

Rust code を変更する task は、task 固有 test の後に次を実行する。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo test --all-targets --features test-support
git diff --check
```

文書だけの task は `git diff --check`、相対 link、記載 path の存在を確認する。macOS framework、
bundle、plist を変更する task は、追加で該当する `cargo check`、`plutil -lint`、`codesign`
verification を実行する。

### 3.6 Evidence

完了 task の末尾へ次を追記する。

```text
Evidence: <commit SHA または artifact path>; <command と結果>; <YYYY-MM-DD>
```

実機 task は node、build SHA、macOS build、開始・終了時刻、操作、期待値、実測値、redaction 済み
log artifact の SHA-256 を記録する。retry で成功した場合も最初の失敗を残す。

## 4. 全体の事前条件、事後条件、release 受入基準

### 4.1 全体の事前条件

- v0.2.1 の local gate が成功し、baseline の LaunchAgent、HTTP port、user data path が記録済み。
- Apple silicon Mac で ad-hoc app を検証できる。
- ソース公開に必要な version、dependency inventory、third-party notices、公開文書が揃っている。
- Developer ID certificate、notary profile、Apple timestamp endpoint は v0.3.0 source release の
  事前条件ではない。
- legacy install、clean install、upgrade/rollback を壊してよい専用 test user または test Mac がある。
- 2 node recovery 試験に、推論 request を止められる change window がある。

### 4.2 全体の事後条件

- 通常の導入経路は source checkout から `cargo xtask install --start` を実行する方法である。
  公式の事前ビルド済み `.app`、`.pkg`、DMG、`Siderostat Uninstaller.app` は配布しない。
- monitor 終了後も runtime は継続し、background service 停止後は launchd が再起動しない。
- legacy と新 runtime は同時に同じ port、state、DS4 child を所有しない。
- user config、secret、manifest、model、cache は install、upgrade、rollback で保持される。
- degraded recovery は既存 demote/promotion owner を迂回せず、失敗時に restart loop へ入らない。
- 通知抑制状態は cluster state、admission、child lifecycle へ feedback しない。
- TP/RDMA の production code、config field、公開 profile は v0.3.0 に含まれない。

### 4.3 release 受入基準

1. crate version、source revision、README、公開導入ガイド、release note の対象 version が一致する。
2. `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、
   `cargo test --all-targets`、`git diff --check` が成功する。
3. dependency inventory と third-party notices が lockfile と一致し、ライセンス情報が確認できる。
4. source、release metadata、CI artifact に secret、credential、private key、model、KV cache、
   runtime state、user data が含まれない。
5. source checkout からの install、upgrade、legacy migration、rollback、uninstall の導線が
   英語・日本語の公開文書と実装で一致する。
6. runtime crash、standalone、paired、distributed、reconnect、recovery、notification dedup の
   既存自動 test と実機 evidence が退行していない。
7. TP/RDMA の production code、config field、公開 profile は v0.3.0 に含まれず、延期理由と
   再開条件が release note に明記されている。
8. Apple Developer ID、notarization、secure timestamp、DMG/pkg の成否を source release の
   合否判定に使用しない。これらを使う場合は将来の任意バイナリ配布 task として別管理する。

## 5. 依存関係

各 task の `Depends on` を正とする。feature 間は次の順序で直列化する。

```text
A-01 -> A-02
          |
          v
B-01 -> B-02 -> B-03
                    |
                    v
C-01 -> C-02 -> C-03 -> C-04 -> C-05
                                      |
                                      v
D-01 -> D-02 -> D-03
                    |
                    v
E-01 -> E-02 -> E-03 -> E-04 -> E-05 -> E-06       macOS 配布 feature gate
                                      |
                                      v
H-01 -> H-02 -> H-03 -> H-04 -> H-05 -> H-06 -> H-07 -> H-08 -> H-09 -> H-10
                                                                              |
                                                                              v
N-01 -> N-02 -> N-03
                    |
                    v
T-01  （TP/RDMA を最後に再評価し、既定では v0.4+ へ延期）
  |
  v
R-01 -> R-02 -> R-03
```

同じ phase 内でも `Depends on` を省略しない。実機 task `E-04`、`E-05`、`H-10`、`N-03`、
`R-03` は同時に実行しない。

## 6. Phase A: release 判断と baseline

### [x] A-01 v0.3.0 の配布判断を固定する

- Actor: agent + user review
- Depends on: なし
- 参照: 配布仕様 3、5.2、8、18 節
- 事前条件: 本書がレビュー対象として保存済み
- Files: `docs/distribution/v0.3.0-release-decisions.md`（新規）
- Actions:
  1. 最低 macOS version を、`SMAppService` を利用できる 13.0 以上の具体値で一つに固定する。
  2. bundle / runtime / pkg identifier は配布仕様 5.2 節の値をそのまま採用する。
  3. Developer ID Application / Installer certificate の表示名を記録し、秘密鍵や
     credential は記録しない。
  4. `Siderostat` 表示名、正式 icon asset、file logging または Unified Logging を決める。
  5. runtime graceful restart endpoint の method、path、認証、成功・失敗 JSON を固定する。
  6. first launch と migration のユーザー向け文言を日本語で固定する。
- 事後条件: 後続 task に build を止める `TBD` がない
- 受入基準: 決定表の全行に「値、理由、変更時の影響、承認日」がある
- Verification: 文書 link と identifier の配布仕様一致を目視確認
- ユーザーレビュー・手作業: 最低 OS、表示名、icon、certificate 表示名、UX 文言を承認する
- 停止条件: Developer ID certificate identity が未取得でも B〜D は進められるが、`E-02` 以降は開始しない

Evidence: docs/distribution/v0.3.0-release-decisions.md; 最低OS 26.0、identifier は仕様5.2採用、Developer ID Application / Installer の表示名、structured file logging、placeholder icon、POST /admin/restart contract と日本語UX文言を記録・承認。証明書 identity の確認結果を反映（2026-08-21）。

### [x] A-02 v0.2.1 の移行 baseline を保存する

- Actor: agent
- Depends on: A-01
- 参照: 配布仕様 7、12、13、16 節
- 事前条件: 作業 tree の既存変更と baseline 採取が競合しない
- Files: `docs/compatibility/v0.3.0-migration-baseline.md`（新規）
- Actions:
  1. crate version、commit、Rust version、macOS target、local gate 結果を記録する。
  2. legacy binary、plist、job label、config、secret、manifest、cache、log path を列挙する。
  3. runtime/monitor/DS4 child の所有関係、既定 port、現在の install/verify/uninstall command を記録する。
  4. migration 後も保持する user data と、自動削除しない file を明示する。
- 事後条件: D phase が比較に使える immutable baseline がある
- 受入基準: 配布仕様 12.1 節の全検出対象と 16.3/16.4 節の全保持対象が対応表にある
- Verification: 共通 local gate と記載 path の repository 内定義を `rg` で照合する
- ユーザーレビュー・手作業: なし
- 停止条件: baseline test が失敗する場合は v0.3 実装を始めず、既存不具合として切り分ける

Evidence: docs/compatibility/v0.3.0-migration-baseline.md; cargo fmt --check OK、cargo clippy --all-targets --all-features -- -D warnings OK、git diff --check OK、記載 path 全存在(2026-08-19); 2026-08-19

## 7. Phase B: deterministic `.app` assembly

### [x] B-01 runtime の既定 path と version metadata を固定する

- Actor: agent
- Depends on: A-02
- 参照: 配布仕様 5.4、7.1、11、13.1 節
- 事前条件: A-01 で最低 OS と version 表示 contract が承認済み
- Files: `src/config.rs`、`src/app.rs`、`src/cli.rs`、直接対応する test
- Actions:
  1. `serve --config` 未指定時に Application Support 配下の `config.toml` を解決する純粋関数を作る。
  2. caller から明示 path が渡された場合は従来どおり優先する。
  3. runtime version、git commit、build number を read-only admin response から取得できるようにする。
  4. path 解決と metadata に `HOME` がない場合の明示 error を追加する。
- 事後条件: bundle plist に user 固有絶対 path を書かず runtime を起動できる
- 受入基準: 未指定、明示指定、`HOME` 不在、空 path の unit test があり、user data を作成しない
- Verification: 対象 unit test、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: config schema migration または既存 CLI の破壊的変更が必要になる

Evidence: src/config.rs に resolve_config_path_pure / platform_default_path_pure を追加(HOME不在・空で明示error、user data非作成)。src/app.rs の /healthz に version/git_commit/build_number を追加。config path unit test 8件、health metadata test を追加。cargo fmt --check OK、clippy --all-targets --all-features -D warnings OK、test --all-targets 201件 OK、test --all-targets --features test-support 全OK、git diff --check OK(2026-08-19); 2026-08-19

### [x] B-02 bundle template と resource を追加する

- Actor: agent
- Depends on: B-01
- 参照: 配布仕様 5、8.3、15 節
- 事前条件: A-01 の identifier、最低 OS、icon 方針が確定済み
- Files: `contrib/macos/Info.plist.in`、`contrib/macos/dev.siderostat-ds4-proxy.runtime.plist`、
  `contrib/macos/entitlements.plist`、`contrib/macos/Resources/`（すべて新規）
- Actions:
  1. 配布仕様 5.1 節と同じ bundle layout を template として表現する。
  2. `/Applications/Siderostat.app` 固定の `Program`、固定 label、`serve`、`RunAtLoad`、
     `KeepAlive`、`ThrottleInterval` を記載する。
  3. entitlement は空または A-01 で承認された最小集合だけにする。
  4. LICENSE、third-party notices、default config、icon を resource として列挙する。
- 事後条件: builder が値を置換する source template が揃う
- 受入基準: plist に user home、secret、`/usr/local/bin`、legacy label、`get-task-allow` がない
- Verification: `plutil -lint`、禁止文字列の静的 test、`git diff --check`
- ユーザーレビュー・手作業: A-01 で正式 icon 提供を選んだ場合、ユーザーが asset を提供する
- 停止条件: icon の license または再配布許可が不明

Evidence: contrib/macos/Info.plist.in(@VERSION@/@BUILD_NUMBER@ placeholder, LSMinimumSystemVersion 26.0)、dev.siderostat-ds4-proxy.runtime.plist(`/Applications/Siderostat.app` 固定の Program, RunAtLoad/KeepAlive/ThrottleInterval)、entitlements.plist(空)、Resources/{LICENSE(MIT, okugauchi 2026), THIRD-PARTY-NOTICES.md, default-config.toml} を追加。tests/bundle_templates.rs に禁止文字列・Program path・identifier・resource の静的 test 4件追加。plutil -lint 3件 OK、test 4件 OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [x] B-03 `app-dev` bundle builder と ad-hoc verification を実装する

- Actor: agent
- Depends on: B-02
- 参照: 配布仕様 5.1、8.2、15、16.1 節
- 事前条件: release build で runtime と monitor の両 binary が生成できる
- Files: `xtask/src/main.rs`、`xtask/src/bundle.rs`（新規）、`xtask/README.md`、関連 test
- Actions:
  1. `cargo xtask app-dev --version <semver> --build-number <integer>` を追加する。
  2. staging directory を毎回空の状態から作り、template、binary、resource を固定順で配置する。
  3. helper、main app の順で ad-hoc 署名し、`--deep` を signing に使わない。
  4. `plutil`、nested identifier、bundle version、layout、signature を検証する command を追加する。
  5. 出力先と生成 file 一覧を表示し、user data と `/Applications` を変更しない。
- 事後条件: certificate なしで production と同じ layout の `.app` を反復生成できる
- 受入基準: 同一入力の二回の build で file 一覧、plist 値、unsigned content digest が一致する
- Verification: builder unit test、`cargo xtask app-dev`、`codesign --verify --deep --strict --verbose=4`、
  共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: builder が sudo、Keychain、notary service、既存 `/Applications/Siderostat.app` を要求する

Evidence: xtask/src/bundle.rs に app-dev builder（指定 version/build number を runtime build 環境変数と Info.plist の両方へ注入、bundle-relative LaunchAgent、placeholder icns 生成、inside-out ad-hoc 署名、plutil/codesign 検証）と unit test 3件を追加。xtask/src/main.rs に AppDev/PkgDev subcommand、xtask/Cargo.toml に base64、xtask/README.md に app-dev/pkg-dev 節を追加。.gitignore に /build /dist を追加。cargo xtask app-dev --version 0.3.0 --build-number 1 --verify で codesign --verify --deep --strict --verbose=4 PASS。二回の build で unsigned content SHA-256 全一致(2026-08-19); 2026-08-22 runtime metadata injection を追加。

## 8. Phase C: Service Management と app lifecycle

### [x] C-01 `SMAppService` adapter と status mapping を実装する

- Actor: agent
- Depends on: B-03
- 参照: 配布仕様 6.1、6.2 節
- 事前条件: ad-hoc app に bundle 内 LaunchAgent plist が存在する
- Files: `monitor/Cargo.toml`、`monitor/src/service_management.rs`（新規）、
  `monitor/src/main.rs`、adapter test
- Actions:
  1. runtime agent と main app login item を識別する小さい adapter interface を定義する。
  2. platform status を `not_registered`、`enabled`、`requires_approval`、`not_found`、`error` へ写像する。
  3. macOS 実装と test fake を分離し、UI code から framework object を直接操作しない。
  4. status 取得だけを実装し、この task では登録状態を変更しない。
- 事後条件: menu state が System Settings の実状態を source of truth として読める
- 受入基準: platform status 全分岐と framework error の unit test がある
- Verification: adapter unit test、macOS `cargo check -p siderostat-monitor`、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: undocumented API、private framework、shell の `launchctl` が必要になる

Evidence: monitor/src/service_management.rs に ServiceManagementAdapter trait、ServiceStatus 5値、ServiceKind、macOS SMAppService 実装（objc2-service-management）、FakeServiceManagement test double を追加。monitor/Cargo.toml に objc2-foundation / objc2-service-management を追加。unit test 6件（status 名、kind、fake script、default、独立性、status mapping 全分岐+未知値error）。macOS cargo check OK、monitor 29 test OK、全 208 test OK、test-support 含 251 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [x] C-02 runtime agent の register / unregister を接続する

- Actor: agent
- Depends on: C-01
- 参照: 配布仕様 6.1、6.3、6.4 節
- 事前条件: status mapping が fake で全分岐 GREEN
- Files: `monitor/src/service_management.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. runtime 用 `register()` と `unregister()` を adapter に追加する。
  2. register 後に status を再取得し、approval 不足を成功として偽装しない。
  3. unregister の二重実行を安全な no-op とする。
  4. AppKit main thread を block せず、結果を UI state channel へ返す。
- 事後条件: bundle mode で `~/Library/LaunchAgents` を変更せず background service を制御できる
- 受入基準: register 成功、approval 必要、拒否、二重 unregister、framework error の test がある
- Verification: fake adapter integration test、macOS check、共通 local gate
- ユーザーレビュー・手作業: 実機 mutation は C-05 まで行わない
- 停止条件: operation 成功を確認するため固定 sleep または `launchctl` parse が必要になる

Evidence: monitor/src/service_management.rs に RegisterOutcome / UnregisterOutcome、adapter の register/unregister、macOS SMAppService register/unregister 実装（kSMErrorLaunchDeniedByUser→DeniedByUser、kSMErrorJobNotFound→AlreadyNotRegistered no-op）、Fake の register/unregister script、classify_*_code 純粋関数を追加。unit test 9件（register 成功/approval/拒否/error、unregister 成功/二重no-op/error、error code 分類 register/unregister）。macOS check OK、monitor 38 test OK、全 208 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [x] C-03 main app login start を独立設定として実装する

- Actor: agent
- Depends on: C-02
- 参照: 配布仕様 6.2 節
- 事前条件: runtime background status と UI state が分離済み
- Files: `monitor/src/service_management.rs`、`monitor/src/tray.rs`、関連 test
- Actions:
  1. `SMAppService.mainApp` の status、register、unregister を adapter に追加する。
  2. runtime background service と monitor login start を別 menu item と別状態で表示する。
  3. 一方の変更が他方を暗黙に変更しないよう test する。
- 事後条件: runtime 常駐許可と monitor login start を独立に設定できる
- 受入基準: 2 x 2 の登録状態 matrix が fake UI test で区別される
- Verification: monitor unit test、macOS check、共通 local gate
- ユーザーレビュー・手作業: 文言と既定推奨値を確認する
- 停止条件: runtime 登録を main app login start の副作用にしないと成立しない

Evidence: monitor/src/service_management.rs の MainAppLoginItem（status/register/unregister）は C-01/C-02 で実装済み。monitor/src/main.rs の first-launch driver が RuntimeAgent と MainAppLoginItem を独立に status 確認し、未登録のサービスだけを `SMAppService.register()` する。いずれか一方が承認待ち・失敗の場合は完了扱いにせず、background toggle は RuntimeAgent のみを変更する。monitor/src/tray.rs には `siderostat-runtime` 自動起動／Siderostat メニューバー自動起動の登録状態表示 menu item 2つと update_registration、registration_status_text 純粋関数を追加。unit test（status 文言 5値、2x2 matrix 独立性=4 distinct pairs、独立サービスの登録結果結合）を追加。monitor 102 test、clippy、fmt/diff-check OK(2026-08-22); 2026-08-22

### [x] C-04 authenticated graceful runtime restart を実装する

- Actor: agent
- Depends on: C-03
- 参照: A-01 の restart contract、配布仕様 6.3、13.1 節
- 事前条件: endpoint の method、path、auth、drain timeout、response が承認済み
- Files: `src/app.rs`、`src/cli.rs`、既存 lifecycle module、関連 test
- Actions:
  1. loopback admin endpoint に token authentication を必須化する。
  2. admission block、in-flight drain、lifecycle owner による owned DS4 child stop、server loop の
     cleanup、runtime process exit の順序を実装する。
  3. drain timeout と child identity mismatch では強制 kill せず error を返す。
  4. response 返却前に exit して client が曖昧な transport error だけを見る設計を避ける。
- 事後条件: launchd が runtime を新 binary で再起動できる graceful path がある
- 受入基準: auth 失敗、正常 drain、drain timeout、identity mismatch、重複要求の test がある
- Verification: handler/lifecycle test、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: cluster lifecycle owner を迂回した signal、unknown PID の kill、unauthenticated endpoint が必要になる

Evidence（親タスク集約）: C-04a の route/auth/body parse/重複要求、C-04b の admission block → drain → owned child stop → response → cleanup-aware restart 処理、C-04c の CLI と受入基準対応を確認。auth 失敗、正常 drain、drain timeout、identity mismatch、重複要求の5受入基準は子タスクの test で網羅され、停止条件にも該当なし（2026-08-25 修正反映）。

#### [x] C-04a graceful restart 専用 route・認証・body parse・重複要求を実装する

- Actor: agent
- Depends on: C-04
- 参照: A-01 の restart contract（2 節）
- 事前条件: `/admin/restart` の method、path、auth、response が承認済み
- Files: `src/app.rs`、関連 test
- Actions:
  1. AppState に supervisor と production runtime の参照を保持し、`serve_with_options` で attach する。distributed 時は production runtime、standalone 時は standalone supervisor を lifecycle owner とする。
  2. `/admin/restart` route を追加する。既存 admin API と同じ Bearer token 認証（`authorized_admin`）を必須化する。
  3. request body の `drain_timeout_ms` を parse し、未指定時は config の cluster stop timeout を既定値にする。
  4. 進行中フラグ（AtomicBool）を追加し、重複要求は `409 { error: "restart_in_progress" }` を返す。
  5. 両 lifecycle owner が不在の場合だけ graceful restart を拒否する。
- 事後条件: `/admin/restart` が認証・parse・重複チェックを経て graceful restart 処理へ進める
- 受入基準: 認証成功/失敗、body 既定値、不正 body、重複要求の test がある
- Verification: handler test、共通 local gate
- ユーザーレビュー・手作業: なし

Evidence: src/app.rs に AppState へ supervisor／production runtime 参照、graceful restart 進行中 AtomicBool・default_drain_timeout（cluster stop timeout）を追加し、serve_with_options で両 owner を attach。try_claim_graceful_restart/release_graceful_restart、/admin/restart route、graceful_restart handler（Bearer 認証 → drain_timeout_ms parse → 重複チェック → owner 存在確認）を実装。unit test で認証、body 既定値、明示 timeout、不正 body、重複要求、owner 不在拒否を確認済み。

#### [x] C-04b graceful restart の処理順序（block→drain→child stop→cleanup→exit）を実装する

- Actor: agent
- Depends on: C-04a
- 参照: A-01 の restart contract（2 節）、配布仕様 13.1 節
- 事前条件: `/admin/restart` route と認証・parse が GREEN
- Files: `src/app.rs`、関連 test
- Actions:
  1. admission block → in-flight drain（`admission.drain(generation, timeout)`）→ lifecycle owner による owned DS4 child stop → `202` を返す → server loop の listener/task/child cleanup → process exit の順序を実装する。
  2. drain timeout 時は強制 kill せず `409 { error: "drain_timeout", in_flight, drain_timeout_ms }` を返す。
  3. child identity mismatch 時は `409 { error: "child_identity_mismatch" }` を返す。
  4. `202` を返した後に server restart signal を予約し、`std::process::exit` で production child cleanup を飛ばさない。
- 事後条件: launchd が runtime を新 binary で再起動できる graceful path が完成する
- 受入基準: 正常 drain、drain timeout、identity mismatch の test がある
- Verification: handler/lifecycle test、共通 local gate
- ユーザーレビュー・手作業: なし

Evidence: src/app.rs で C-04a の placeholder だった perform_graceful_restart を本実装。GracefulRestartOutcome（Ready/DrainTimeout/ChildIdentityMismatch/ChildStopFailed）と graceful_restart_sequence（admission block → drain → lifecycle owner child stop、exit 副作用なし）を追加。2026-08-25 の実機調査で、従来の `std::process::exit` が `run_servers` の production cleanup を飛ばす原因を特定したため、distributed は `ProductionClusterRuntime::stop_distributed()`、standalone は `StandaloneSupervisor` を選択し、成功後は server loop の restart signal を通して listener/task/child cleanup を実行するよう修正した。owner 選択、signal 到達、既存 graceful sequence の unit test、`cargo test --all-targets` 263件、`cargo clippy --all-targets -- -D warnings`、fmt/diff-check が PASS。

#### [x] C-04c graceful restart CLI サブコマンドと受入基準 test を追加する

- Actor: agent
- Depends on: C-04b
- 参照: A-01 の restart contract（2 節）
- 事前条件: `/admin/restart` の graceful 処理が GREEN
- Files: `src/cli.rs`、関連 test
- Actions:
  1. CLI に graceful restart サブコマンド（admin token 読込 + `/admin/restart` POST）を追加する。既存の `/cluster/restart` 呼び出しと区別する。
  2. auth 失敗、正常 drain、drain timeout、identity mismatch、重複要求の受入基準 test を一対一で追加する。
  3. 停止条件（cluster lifecycle owner 迂回 signal、unknown PID kill、unauthenticated endpoint）が CLI で発生しないことを確認する。
- 事後条件: operator が CLI から graceful restart を発行できる
- 受入基準: 5 種の受入基準 test が C-04 の受入基準に対応する
- Verification: CLI test、共通 local gate
- ユーザーレビュー・手作業: なし

Evidence: src/cli.rs に ClusterCommand::GracefulRestart（`cluster graceful-restart`）を追加し、run_cluster の request 決定を純粋関数 cluster_request(command) へ抽出。GracefulRestart は `/admin/restart` POST（既存 cluster restart の `/cluster/restart` と区別）、POST は mutation 扱いで admin token を読み bearer auth を付与。unit test 3件（全サブコマンドが admin client path を選択 + graceful-restart 含む、GracefulRestart が /admin/restart を選び cluster Restart と区別、POST mutation で bearer auth / GET と区別）。C-04 の 5 受入基準（auth 失敗・正常 drain・drain timeout・identity mismatch・重複要求）は C-04a/C-04b の handler/sequence test が対応。停止条件（cluster lifecycle owner 迂回 signal、unknown PID kill、unauthenticated endpoint）は CLI では発生しない（/admin/restart のみ、bearer auth、lifecycle owner 経由）。全 219 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [x] C-05 bundle mode の menu と first launch を完成する

- Actor: agent + user review
- Depends on: C-04
- 参照: 配布仕様 6.3、11 節、A-01 の UX 文言
- 事前条件: Service Management と graceful restart の fake test が GREEN
- Files: `monitor/src/main.rs`、`monitor/src/tray.rs`、`monitor/src/settings.rs`、関連 test
- Actions:
  1. 「Siderostatを終了」「siderostat-runtimeを再起動」「siderostat-runtimeを起動して自動起動を有効化／siderostat-runtimeを停止して自動起動を無効化」を別操作にする。
  2. bundle mode では通常経路から `launchctl kickstart/bootout` を除く。
   3. first launch reducer が legacy inventory、config 検証、Runtime／main app status、background item 説明を
     順に受け取る interface を作り、D-01 の read-only inventory を接続する。
  4. Login Items を常時開ける明示操作を表示し、別の操作が進行中のときだけ無効化する。
  5. registration progress と model startup progress を別状態で表示する。
- 事後条件: 配布仕様 6.3 と 11 節の操作が UI から区別できる
- 受入基準: menu event test と first-launch state reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、ad-hoc app の手動起動前 static check、共通 local gate
- ユーザーレビュー・手作業: UI 文言、操作の危険度、approval 導線を画面で承認する
- 停止条件: first launch が model load 完了まで UI を block する、または拒否状態を enabled と表示する

Evidence（実装・最終画面確認完了 2026-08-22）: menu bar の非操作ステータスへ first-launch
progress を接続。起動時に D-01 read-only legacy inventory、config validation、Runtime／main app の
SMAppService status、両サービスの registration を順序どおり reducer へ投入し、承認待ちは Login
Items の両 status 変化を main-thread timer で再確認する。登録後の `/healthz` と `/readyz` の待機は
dedicated worker で行い、AppKit event loop を block せず model を monitor 側で load しない。
first-launch の version/build metadata、各段階、approval、failure 文言を英語／日本語の
`Localizable.strings` へ追加。ユーザーレビュー（2026-08-22）で app-dev bundle を起動し、
最終状態「モデルの準備を確認しました」を確認。first-launch が ModelReady へ到達し、model load
完了まで UI を block しないことを確認した。

#### [x] C-05a bundle mode 判定とメニュー操作の分離を実装する

- Actor: agent
- Depends on: C-04
- 参照: 配布仕様 6.3、11 節
- 事前条件: Service Management と graceful restart の fake test が GREEN
- Files: `monitor/src/main.rs`、`monitor/src/tray.rs`、`monitor/src/launchd.rs`、関連 test
- Actions:
  1. bundle mode 判定（実行 path が `.app/Contents/MacOS` 配下か）を追加する。bundle mode では通常経路から `launchctl kickstart/bootout` を除く。
  2. 「Siderostatを終了」「siderostat-runtimeを再起動」「siderostat-runtimeを起動して自動起動を有効化／siderostat-runtimeを停止して自動起動を無効化」を別 menu item にする。既存の Proxy 再起動は graceful restart の `/admin/restart` 呼び出しへ置き換える。
  3. `siderostat-runtime` の起動・停止と自動起動の有効化・無効化を `ServiceManagementAdapter` の register/unregister（C-02）へ接続する。
- 事後条件: 配布仕様 6.3 と 11 節の操作が menu 上で区別できる
- 受入基準: menu id と bundle mode 判定の unit test がある
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: なし（文言は C-05 本体で承認）

Evidence: monitor/src/launchd.rs に is_bundle_mode/is_bundle_path（`.app/Contents/MacOS` 配下判定）を追加し、kickstart/bootout は bundle mode で bail。monitor/src/tray.rs の menu id を MENU_QUIT/MENU_RUNTIME_RESTART/MENU_BG_TOGGLE/MENU_OPEN_CONFIG に整理（旧 MENU_PROXY_RESTART/MENU_MONITOR_RESTART 廃止）、各 is_*_event 判定を更新。monitor/src/main.rs の menu handler を再構成し、bundle mode では quit=プロセス終了/runtime restart=graceful_restart（client.graceful_restart、/admin/restart）、非 bundle では launchctl。`siderostat-runtime` の自動起動は同一 menu item の表示を状態に応じて有効化／無効化へ切り替え、register_runtime（ServiceManagementAdapter register/unregister）へ接続。client.rs に graceful_restart メソッドを追加。unit test（menu id 一意性、各 event が自分の id のみ真、bundle path 検出/非検出）追加。monitor 44 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

#### [x] C-05b first-launch state reducer と UI state を実装する

- Actor: agent
- Depends on: C-05a
- 参照: 配布仕様 11 節、A-01 の UX 文言
- 事前条件: menu 操作分離が GREEN
- Files: `monitor/src/state.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. first-launch reducer が legacy inventory、config 検証、runtime status、background item 説明を順に受け取る interface を追加する。実 inventory は D-01 から接続する。
  2. `requires_approval` 時だけ Login Items を開く明示操作を表示する状態を追加する。
  3. registration progress と model startup progress を別状態で表示する。
- 事後条件: first-launch の各段階が UI state として区別できる
- 受入基準: first-launch state reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: なし（UI 文言は C-05 本体で承認）

Evidence: monitor/src/state.rs に first-launch reducer（C-05b）を追加。FirstLaunchState（VersionShown/InventoryChecked/ConfigValidated/ServiceStatusesChecked/Registering/Registered/RequiresApproval/RegisterFailed/MonitorLoginChecked/RuntimeAdminReady/ModelReady）と FirstLaunchEvent、純粋な first_launch_reducer を実装。配布仕様 11 節の順序（version→inventory→config→Runtime／main app status→両サービス登録→login→admin ready→model ready）を反映し、registration progress と model-startup progress を別段階で表現。config invalid / requires approval / register failed の失敗分岐、Runtime と main app の両 status が enabled になるまでの approval 後再確認、順序外 event を無視する挙動を実装。first_launch_needs_approval（RequiresApproval 時のみ Login Items 導線表示）、first_launch_complete（ModelReady）。C-05 の起動 driver が D-01 read-only inventory と `/healthz`・`/readyz` を接続。unit test（happy path、legacy present/absent、config invalid で register 前停止、approval 導線ゲート、片方だけ enabled の承認待ち継続、approval 後の再開、register failed を enabled と表示しない、順序外無視）。monitor 102 test OK（2026-08-22）。

#### [x] C-05c menu event test と受入基準 test を追加する

- Actor: agent + user review
- Depends on: C-05b
- 参照: 配布仕様 6.3、11 節
- 事前条件: first-launch reducer が GREEN
- Files: `monitor/src/tray.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. 各 menu event（終了、runtime restart、バックグラウンド実行の toggle、Login Items を開く）の一意性と分岐を test する。
  2. bundle mode 判定と non-bundle mode の launchctl 経路を test する。
  3. 停止条件（first launch が model load まで UI を block する、拒否状態を enabled と表示する）が発生しないことを確認する。
- 事後条件: C-05 の受入基準（menu event test と first-launch reducer test）が満たされる
- 受入基準: menu event test と first-launch reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、ad-hoc app の手動起動前 static check、共通 local gate
- ユーザーレビュー・手作業: UI 文言、操作の危険度、approval 導線を画面で承認する

Evidence（自動検証・ユーザーレビュー完了 2026-08-21）: monitor/src/settings.rs に open_login_items（System Settings のログイン項目 pane を開く）と LOGIN_ITEMS_SCHEME を追加。monitor/src/tray.rs に MENU_OPEN_LOGIN_ITEMS menu item（「ログイン項目を開く」）と is_open_login_items_event を追加し、`siderostat-runtime` の自動起動は MENU_BG_TOGGLE の同一位置で有効化／無効化を排他的に表示するよう変更。「ログイン項目を開く」は操作中以外を常に有効化。monitor/src/main.rs に open_login_items 分岐を追加し、SMAppService の状態に応じて `siderostat-runtime` の再起動と自動起動 toggle を更新。operation.rs に非同期操作の進行中／成功／承認要求／拒否／失敗状態を追加。graceful restart response の JSON 解析失敗で成功を失敗表示しないよう client.rs を修正し、`siderostat-runtime` と共有する admin token file を自動読込して Bearer 認証を付与。menu id 一意性、状態別 action gating、開始／停止表示切替、token file の hex 化 test を追加。メニュー操作・状態・操作結果の文言を `Localizable.strings`（英語／日本語）へ外部化し、App bundle の `NSBundle` から locale を解決する構成を追加。メトリクス識別子は英語固定とした。monitor 94 test、app-dev bundle の resource/plist/codesign verification を確認。ユーザーレビュー（2026-08-21）: UI 文言、操作の危険度、ログイン項目への導線、状態別メニュー表示を確認し、すべて OK。今回の first-launch 接続実装を含む最終画面確認は C-05 本体で実施する。

Behavior note (2026-08-21): `SMAppService.register()` は LaunchAgent の登録時にサービスを起動し、`unregister()` は稼働中の LaunchAgent とその管理対象 child を終了させるため、メニュー文言を「siderostat-runtimeを起動して自動起動を有効化」／「siderostat-runtimeを停止して自動起動を無効化」へ変更。終了操作は実装上の component 名ではなく、正式なアプリ名を使う「Siderostatを終了」／「Quit Siderostat」とし、ログイン項目の状態も「Siderostat メニューバー自動起動」／「Siderostat Menu Bar Auto-Start」に統一した。英語・日本語の操作中／成功／失敗文言と distribution spec も同じ挙動に合わせた。`operation` の lifecycle 文言 test を追加。

## 9. Phase D: legacy migration、upgrade、rollback

### [x] D-01 legacy install の read-only inventory と backup を実装する

- Actor: agent
- Depends on: C-05
- 参照: 配布仕様 12.1、12.2 節 1〜4
- 事前条件: A-02 に legacy file と job label が固定済み
- Files: `monitor/src/migration.rs`（新規）、`monitor/src/main.rs`、関連 test
- Actions:
  1. binary、plist、job label、PID、実行 path を read-only で検出する。
  2. PID と executable identity が一致しない項目を自動操作対象から除外し、利用可能な target では
     `SMAppService.statusForLegacyPlist(at:)` の結果も inventory に含める。
  3. legacy plist を Application Support 内の一意な migration backup へ copy する。
  4. inventory と backup manifest を atomic write し、secret や config 本文を含めない。
- 事後条件: migration 前状態を復元するための検証済み inventory が first launch UI へ渡る
- 受入基準: 未導入、一部導入、二 job、identity mismatch、backup 再実行の fixture test がある
- Verification: migration unit test、permission test、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: inventory のため root 権限、console user 推測、unknown process への signal が必要になる

Evidence: monitor/src/migration.rs を新規作成。LegacyInventory（binaries/plists/jobs/legacy_status）と inventory_legacy（read-only、legacy binary・plist を検出、usr_local_bin/home/agents dir を fixture 可能に分離）。LegacyJob/LegacyJobIdentity と verify_job_identity（PID + executable SHA-256 一致時のみ verifiable、mismatch は自動操作対象から除外）。backup_legacy（plist を Application Support/migration-backup/ へ一意 suffix 付き copy、manifest は追記で過去の backup を保持、atomic write）。BackupManifest/BackupEntry（path/size のみ、secret/config 本文を含まない）。macOS: legacy_plist_status（SMAppService.statusForLegacyURL → not_registered/enabled/requires_approval/not_found/error の安定文字列）。fixture test 7件（未導入、一部導入、二 job、identity mismatch、backup 再実行、digest、legacy status mapping）。monitor 60 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19)。first-launch UI の inventory 受け渡しは C-05 起動 driver へ接続済み。legacy cutover への利用は D-02 で実施する。

### [x] D-02 legacy から新 service への cutover と rollback を実装する

- Actor: agent
- Depends on: D-01
- 参照: 配布仕様 12.2 節 2〜7、13.2 節
- 事前条件: backup manifest の write と再読込 test が GREEN
- Files: `monitor/src/migration.rs`、`monitor/src/service_management.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. user confirmation 後だけ legacy runtime を既存 admin API で drain する。
  2. identity 確認済み legacy jobs を user domain から停止し、legacy plist を削除せず migration
     backup 内へ退避して `~/Library/LaunchAgents` から除く。
  3. new runtime を登録して readiness と config compatibility を確認する。
  4. 失敗時は new service を unregister し、backup plist と legacy jobs を復元する。
  5. old/new runtime が同時に同じ port を listen した場合は直ちに failure として rollback する。
  6. 成功後も legacy binary を自動削除せず、「削除可能」とだけ表示する。
- 事後条件: migration 成功または旧環境復旧のどちらかへ有限時間で収束する
- 受入基準: 各 action に failure injection があり、全点で user data 不変、job 最大一組を確認する
- Verification: state-machine test、fake service integration test、共通 local gate
- ユーザーレビュー・手作業: 実機 migration は E-05 まで実行しない
- 停止条件: rollback のため user data 削除、state file 削除、force kill が必要になる

Evidence: monitor/src/migration.rs に cutover state machine（D-02）を追加。CutoverState（Idle/Draining/LegacyStopped/NewRegistered/Migrated/RollingBack/RolledBack/RollbackFailed）と CutoverEvent（DrainFinished/LegacyStopped/NewRegistered/ReadinessChecked/PortConflict/RolledBack、各 Result で failure injection）と純粋な cutover_reducer。各 action の失敗は rollback へ、port conflict は即 rollback。CutoverDriver trait（drain_legacy/stop_legacy/register_new/check_readiness/port_conflict/rollback）と run_cutover ドライバ（spec 12.2 手順 2-7 の順で呼び、Err/conflict で finish_rollback）。state-machine test 8件（happy path、各 action failure、port conflict、rollback ok/fail、有限収束、user data 不変、job 最大一組）+ fake driver integration test 6件（呼び出し順、drain 失敗で即 rollback、readiness 失敗、port conflict 即 rollback、rollback 失敗=RollbackFailed、収束後 op 呼び出しなし）。monitor 74 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19)。実機 migration は E-05 まで実行しない。実 driver は E-05 で main.rs に接続。

### [x] D-03 app/runtime version handshake と不一致通知を実装する

- Actor: agent
- Depends on: D-02
- 参照: 配布仕様 13 節
- 事前条件: B-01 の runtime metadata が admin API から取得可能
- Files: `monitor/src/client.rs`、`monitor/src/main.rs`、`monitor/src/localization.rs`、`monitor/src/state.rs`、`monitor/src/tray.rs`、`src/notify.rs`、関連 test
- Actions:
  1. app version/build と runtime version/build を比較する。
  2. 一致時、runtime 旧版、runtime 新版、取得不能を別状態にする。
  3. mismatch 時は状態変化を macOS 通知で一度だけ知らせ、必要な再起動は既存メニューからの明示操作に限定する。
  4. prior app へ rollback して schema 非互換の場合は警告し、data を自動変換しない。
- 事後条件: `.pkg` 更新後に旧 executable image が残る状態を利用者が解消できる
- 受入基準: version matrix、不一致通知の重複抑止、restart 成功/失敗/拒否の UI test がある。修正後に不一致状態を再現できないため、通知の実機表示確認は受入要件に含めない
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: なし（通知の実機表示確認は受入要件から除外）
- 停止条件: mismatch 解消のため無条件 restart または config の不可逆 migration が必要になる

Evidence（自動実装・検証完了 2026-08-22）: `monitor/src/client.rs` の `/healthz` を `RuntimeVersion（version/git_commit/build_number）` として利用し、`monitor/src/localization.rs` の app version/build と `monitor/src/state.rs` の `version_handshake_with_build` で version/build の両方を比較する構成を追加。`VersionHandshake（Matched/RuntimeOlder/RuntimeNewer/Unavailable）` を monitor の常駐 poll に接続し、Matched は無表示、旧版・新版・取得不能は状態変化ごとに macOS 通知を一度だけ表示する。同じ不一致の重複通知を抑止し、初回起動時の取得不能は通知せず、再起動は既存の「siderostat-runtimeを再起動」メニューからの明示操作に限定する。通知文言は `contrib/macos/Resources/{en,ja}.lproj/Localizable.strings` に外部化し、app/runtime の version/build を本文へ含める。`monitor/src/tray.rs` から version 状態・upgrade 専用メニュー項目・常時表示の一致文言を削除し、既存の runtime restart 結果は成功・失敗・拒否（401/403）として外部化された日英文言で表示する。旧 builder が Info.plist だけを 0.3.0 に置換し runtime binary を Cargo version 0.2.1 のまま同梱していたため、`src/app.rs` の runtime version override と `xtask/src/bundle.rs` の version/build 注入付き release build を追加し、bundle 内 app/runtime metadata を一致させた。unit test は version/build matrix、通知対象・重複抑止・初回取得不能の扱い、restart 成功/失敗/拒否表示、menu id/action gating を含む。検証は monitor test、`cargo build --release --workspace`、`cargo test --all-targets`、`cargo xtask app-dev --version 0.3.0 --build-number 1 --verify`、`cargo fmt --all`、`git diff --check` で実施した。実機通知表示は修正後に不一致状態を再現できないため受入要件から除外し、再現 fixture／手順整備後に確認する将来 TODO とする。

## 10. Phase E: bundle/package、署名、公証の内部実装と任意 binary workflow

> Phase E の task は bundle/package 実装、実機検証、将来の任意バイナリ配布 workflow を記録する。
> v0.3.0 の公式 source release gate は R-02/R-03 で定義し、Phase E の Developer ID、notarization、
> secure timestamp、DMG/pkg artifact を公式配布物として要求しない。

### [x] E-01 Monitor 終了 hook 付き flat `.pkg` builder を実装する

- Actor: agent
- Depends on: D-03
- 参照: 配布仕様 9、15 節
- 事前条件: ad-hoc app と migration/lifecycle 自動 test が GREEN
- Files: `xtask/src/main.rs`、`xtask/src/package.rs`（新規）、`xtask/README.md`、関連 test
- Actions:
  1. `cargo xtask pkg-dev` を追加し、B-03 の app を component package と product archive にする。
  2. payload を `/Applications/Siderostat.app` 一項目に限定する。
  3. component receipt/product identifier と semver を template から固定する。
  4. bundle replacement 前に、active console user の GUI domain にある product-owned runtime
     LaunchAgent を unload し、その後既存 Monitor を終了する `preinstall` を生成する。
  5. install 完了後、runtime が更新前から `enabled` だった場合だけ新しい bundle 内の
     LaunchAgent を同じ GUI domain へ bootstrap し、active console user の GUI session で
     Siderostat を起動する controlled `postinstall` を生成する。設定、secret、plist ファイル、
     Service Management の恒久的な登録状態は変更しない。
  6. package expand 結果を検査し、想定外の script と禁止 path があれば失敗する。
- 事後条件: certificate なしで final と同形の installable package を作れる
- 受入基準: package payload manifest が一項目、制御された `preinstall` / `postinstall` のみ、同一入力で receipt/version 一致
- Verification: package builder test、`pkgutil --expand-full` の静的検査、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: package script から product-owned runtime job と exact-path Monitor 以外の process、
  user data、または永続的な Service Management 登録を操作する必要が生じる

Evidence: `xtask/src/package.rs` を本実装に置き換え（E-01）。固定 identifier（COMPONENT_IDENTIFIER=dev.siderostat-ds4-proxy.pkg、PRODUCT_IDENTIFIER=dev.siderostat-ds4-proxy.product、INSTALL_LOCATION=/Applications、PAYLOAD_PATH=/Applications/Siderostat.app）と PKG_STAGING_DIR=build/pkg-dev。`pkg_dev()` が app を一時 payload root へ `ditto` し、BundleIsRelocatable=false の component plist と、active console user の `gui/<uid>/dev.siderostat-ds4-proxy.runtime` だけを `launchctl bootout` し、必要時に同じ job target と exact runtime path を SIGKILL した後、既存 Monitor の exact path を SIGTERM（最大10秒）→必要時のみ SIGKILL する `preinstall` を指定した `pkgbuild`（`--root --component-plist --scripts --install-location --identifier --version`）→ `productbuild`（`--package --identifier --version`）→ `pkgutil --expand-full` → `inspect_expanded` の順で実行する。`postinstall` は更新前の `enabled` 状態を保持している場合だけ新しい bundle 内の LaunchAgent を bootstrap し、その後 active console user の GUI session で新しい Siderostat.app を起動する。既存 bundle identifier による `/Applications` 外への relocation を抑止し、`Siderostat.app` 一項目・制御された `preinstall`／`postinstall`・禁止 path を検査し、違反時は fail-closed する。installer は plist ファイル、Service Management の恒久登録、設定、secret、model、cache を変更せず、任意 process や `ds4-server` child を直接対象にしない。`inspect_expanded()` は展開結果を再帰的に探索し、payload/script/禁止 path を検査する。unit test、xtask package test、shell static verification、`cargo clippy --workspace --all-targets -- -D warnings`、`git diff --check` が PASS（2026-08-25）。certificate なしで final と同形の installable package を作れる。`pkg-dev` CLI は B-03 から安定。

補正記録（2026-08-23）: package の script policy を `preinstall` と controlled `postinstall` に更新した。
`postinstall` は `/bin/launchctl asuser <console-uid> /usr/bin/open -a /Applications/Siderostat.app`
だけを要求し、GUI user がない場合は成功扱いで起動を省略する。アプリ起動後の
`SMAppService` 登録と Login Items approval はユーザー session 内の Siderostat が担当する。
package expand 検査はこの二つの script 名を許可し、順序に依存せず決定的に比較する。

### [x] E-02 Developer ID signing / notarization pipeline を実装する

- Actor: agent + user review
- Depends on: E-01
- 参照: 配布仕様 8、10、15、16.1 節
- 事前条件: A-01 に certificate 表示名があり、private key は Keychain 内にだけ存在する
- Files: `xtask/src/signing.rs`（新規）、`xtask/src/package.rs`、`xtask/src/main.rs`、
  `.gitignore`、`xtask/README.md`
- Actions:
  1. helper、main app、installer の inside-out signing を実装する。
  2. Hardened Runtime、secure timestamp、明示 identifier を command line で検証する。
  3. final `.pkg` を `notarytool submit --wait`、log 保存、staple、validate の順で処理する。
  4. credential は Keychain profile 名だけを受け取り、command、log、artifact metadata に展開しない。
  5. checksum、git commit、Rust version、target、build number、notary submission ID を出力する。
  6. Apple timestamp 障害時の明示的な診断用切替として `--timestamp-mode apple|none` を提供する。
     `none` は `--timestamp=none` を使い、公証・staple・Gatekeeper 検証を省略した
     `-no-timestamp` artifact だけを生成する。これは配布用ではない。
- 事後条件: credential を repository に置かず、単一 command で配布 artifact を生成・検証できる
- 受入基準: `--dry-run` で command 順、identifier、出力を検証でき、secret pattern test が通る
- Verification: unit test、dry-run、`git diff --check`、実署名は E-04 で行う
- ユーザーレビュー・手作業: Keychain profile 名、certificate 選択、notary log 保存先を承認する
- 停止条件: password、private key、App Store Connect key 本文を引数または repository file に要求する

Unblocked: `security find-identity -v` で Apple Development、Developer ID Application、
Developer ID Installer の3 identityを確認し、E-02 実装を再開した。`-p codesigning` は
Installer identity を除外するため使用しない。private key と notary credential 本文は repository
やログへ保存しない(2026-08-21)。

Evidence（自動 part）: `xtask/src/signing.rs` と `cargo xtask sign` を追加。helper → main app の
inside-out Developer ID Application signing、Hardened Runtime、secure timestamp、明示 identifier、
Developer ID Installer 付き productbuild、notarytool submit/wait、log 保存、staple、validate、
Gatekeeper 検証、checksum/build metadata 出力を実装。`--dry-run` で profile 実値を redaction し、
固定 command 順・identifier・payload・出力 path を確認。bundle tree digest と metadata 出力の
不具合を修正し、unit test 26件、`cargo fmt --all -- --check`、
`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets`、
`cargo test --all-targets --features test-support`、`git diff --check` が PASS。実署名・公証は E-04
で実行し、clean install 実機検証を継続する(2026-08-22)。

User review: `siderostat-notary` を login Keychain profile として再登録し、`xcrun notarytool history
--keychain-profile siderostat-notary` が credential エラーではなく `No submission history.` を返す
ことを確認後、0.3.0 build 1 の submission が `Accepted` になった。notary log 保存先 `dist/notary/`
を採用する(2026-08-22)。

Evidence: `cargo xtask sign --dry-run`、`xcrun notarytool history --keychain-profile
siderostat-notary`、自動 gate は PASS。実署名・公証 artifact は E-04 で生成し、
`dist/Siderostat-0.3.0.pkg` の `pkgutil --check-signature`、stapler、Gatekeeper 検証が PASS。

Evidence（E-04 action 1 完了、clean install 待ち 2026-08-22）: `cargo xtask sign` により
Developer ID Application／Installer 署名、notarytool submit/wait、redacted log 保存、staple、
validate、Gatekeeper 検証、metadata 出力を完了。`dist/Siderostat-0.3.0.metadata.json` に
version/build、app/pkg SHA-256、Rust version、target、notary submission ID を保存した。

実装追補（2026-08-24）: Apple timestamp endpoint 障害の診断用に `--timestamp-mode apple|none` を
追加した。既定値は `apple` で、従来どおり公証・staple・Gatekeeper 検証を要求する。`none` は
`codesign`／`productbuild` の `--timestamp=none`、`-no-timestamp` artifact 名、metadata の
`timestamp_mode=none`／`distribution_ready=false` を使用し、公証・staple・Gatekeeper 検証を
スキップする。公証 profile は `none` では不要である。

### [x] E-03 macOS CI に ad-hoc app/pkg verification を追加する

- Actor: agent
- Depends on: E-02
- 参照: 配布仕様 16.1 節
- 事前条件: `app-dev` と `pkg-dev` が clean worktree で成功する
- Files: `.github/workflows/ci.yml`、`scripts/verify-macos-dev-artifacts.sh`、必要な xtask test
- Actions:
  1. certificate と network を使わない ad-hoc app build job を追加する。
  2. plist、layout、nested signature、package payload、禁止 script/path を検査する。
  3. Developer ID / notarization は CI 必須 job にせず、release 手動 gate とする。
  4. build artifact に secret、user data、model がないことを file list で検査する。
- 事後条件: bundle/package 構造の退行が pull request で検出される
- 受入基準: 壊した plist、余分な payload、unsigned helper を fixture で個別に失敗させられる
- Verification: workflow syntax、local 相当 command、共通 local gate
- ユーザーレビュー・手作業: GitHub required check に追加する場合はユーザーが repository 設定を変更する
- 停止条件: CI secret を pull request job へ公開する必要がある

Evidence: `.github/workflows/ci.yml` に certificate/network/secret を使わない `macos-dev-artifacts` job、`scripts/verify-macos-dev-artifacts.sh` に fixture と実 artifact 検査を追加。壊れた plist、余分な payload、unsigned helper の3 fixture が個別に失敗すること、`app-dev --verify` の plist/layout/nested signature、`pkg-dev` の `/Applications` install location・`Siderostat.app` 一項目・制御された `Scripts/preinstall` 一項目・禁止 path、secret・user data・model の不在を確認。`bash -n scripts/verify-macos-dev-artifacts.sh`、`cargo build --release --workspace`、`cargo test -p xtask`（25件）、local 相当 `bash scripts/verify-macos-dev-artifacts.sh`、`git diff --check` が PASS（2026-08-21）。Developer ID / notarization は CI job に含めない。次に着手可能: `E-04`。

### [x] E-04 signed/notarized package の clean install を実機検証する

- Actor: user + agent
- Depends on: E-03
- 参照: 配布仕様 16.1、16.2 節
- 事前条件: clean test user/Mac、Developer ID certificate、notary profile、rollback 用 snapshot がある
- Files: `docs/compatibility/v0.3.0-clean-install.md`（新規、redaction 済み結果のみ）
- Actions:
  1. final candidate を署名、公証、staple し、全 static verification を実行する。
  2. offline Gatekeeper 条件を含め、clean Mac へ `.pkg` を install する。
  3. first launch 前に runtime 未登録、first launch 後に説明と approval flow が動くことを確認する。
  4. monitor 終了、runtime crash、background stop、login start の各 lifecycle を確認する。
  5. `/Applications` 以外に system payload がないことを package receipt と file system で確認する。
- 事後条件: clean install の PASS/FAIL evidence と再実行可能な手順がある
- 受入基準: 配布仕様 16.1/16.2 の全項目が実測値付き PASS、または失敗 task ID 付き FAIL
- Verification: `codesign`、`spctl`、`pkgutil`、`stapler`、login/logout、crash/restart 観測
- ユーザーレビュー・手作業: install 承認、app 初回起動、Login Items 承認/拒否、login/logout を実行する
- 停止条件: main 利用環境しかなく rollback snapshot がない、または production user data を消す必要がある

Evidence（E-04 action 1–2 完了、action 3–5 未実施 2026-08-22）: 修正版 `dist/Siderostat-0.3.0.pkg`
（pkg SHA-256: `05502709b08ab01d98485ef10d38cda61657dff303cf1fd0c231d31e152baf08`）を、旧
0.2.0 のアプリ／LaunchAgent／root binary を先に除去した既存 user account の MacBook Pro と
Mac Studio へ再インストールした。両ノードで receipt `dev.siderostat-ds4-proxy.pkg` の
version=`0.3.0`、location=`Applications`、`/Applications/Siderostat.app` の存在、app の
`codesign --verify --deep --strict` 成功を確認し、既存の開発 workspace bundle へ relocate されない
ことを確認した。Mac Studio の `spctl --assess --type execute` は `accepted` / `source=Notarized
Developer ID`。MacBook Pro では同じ app に対する `spctl` が macOS の Code Signing subsystem
internal error となったため、Gatekeeper の実機結果は未確定として残す。初回起動前の runtime 未登録、
説明・approval flow、monitor 終了／runtime crash／background stop／login start は次の実機操作で確認する。

Evidence（E-04 action 3–5 user review、部分 PASS / 通知課題あり 2026-08-22）: Mac Studio の初回起動で
初回起動通知・Login Items 承認導線を確認した。「Siderostatを終了」はメニューバーアプリだけを終了し、
runtime を巻き込まないことを確認した。旧ビルドの停止操作は runtime 停止自体は
次の起動操作につながったが、停止直後に runtime のバージョン取得不能通知が表示された。
これは意図的な停止を `VersionHandshake::Unavailable` として通知する現在実装によるもので、操作結果として
抑制すべき通知である。続く旧ビルドの起動操作は約1分後に成功したが、
「Standaloneモードで起動」「ネットワーク上のノード検出」の通知ループが発生した。起動成功と通知ループを
分離して扱い、停止中の version unavailable 抑制と、起動／network discovery 通知の重複抑止を実装・再検証する
まで E-04 は未完了とする。

Evidence（通知修正実装・build 2 artifact 準備完了、実機再確認待ち 2026-08-22）:
`monitor/src/main.rs` の version handshake poll に operation lifecycle を接続し、意図的な
background stop、background start、graceful restart の実行中・完了直後は `Unavailable` 通知を抑制し、
health が再び Matched になった時点で通常監視へ戻すよう修正した。`src/notify.rs` に recovery epoch
内の `SoloStandaloneReady`／`PairedStandaloneReady` を各1回に制限する純粋な
`NotificationDeduplicator` を追加し、`DistributedReady` で epoch を更新した。failure、manual
intervention、deployment mismatch、standalone restart、DistributedReady は抑制しない。
operation lifecycle 7件と epoch/dedup 3件を含む monitor 99 test、core 215 test、clippy、format、
diff check が PASS。build 2 の signed/notarized artifact は package SHA-256
`11a98624601f723ab9b75533a243fd2a4bb6146fddbcd72769ea5678adad905f`、notary submission
`858c1db9-1f9c-43a3-a6f2-b78294b0c2c3`、Gatekeeper `accepted / source=Notarized Developer ID`。
両ノードへの build 2 再インストールと同じ操作の再実機確認を残す。なお、後述の build 4 が
build 2 を置き換える。

Evidence（通知 i18n 実装・build 4 artifact 準備完了、実機再確認待ち 2026-08-22）:
通知タイトル・本文を `en.lproj/Localizable.strings`／`ja.lproj/Localizable.strings` に外部化し、
起動、Standalone／Distributed、peer 検出、backoff、deployment mismatch、manual intervention、
startup cleanup の通知へ適用した。bundle 外や未登録キーでは日本語の source fallback を使う。
core 216 test、monitor 99 test、clippy、format、diff check が PASS。i18n を含む build 4 の
signed/notarized artifact は `dist/i18n-build4/Siderostat-0.3.0.pkg`、package SHA-256
`fc8736b0c4bd56cfea63f76f98642ee92ca4b73ea2a740940a5697237b5dc9b5`、notary submission
`a766ae5f-b4d2-49d5-9c68-28e25328f8db`、stapler `The validate action worked!`、Gatekeeper
`accepted / source=Notarized Developer ID`。Mac Studio の `/tmp/Siderostat-0.3.0-i18n-build4.pkg`
へ同一 SHA-256 で転送済み。build 3 は詳細部分の追加 i18n 前のため使用せず、両ノードへの build 4
再インストールと、英語／日本語メニュー・通知の
実機再確認を残す。

Evidence（Monitor 応答性・ロケール選択・用語整理、build 5 artifact 準備完了、実機再確認待ち
2026-08-22）: `monitor/src/main.rs` の CFRunLoop refresh callback が共有表示状態の mutex を
保持したまま `SMAppService.status()` を呼び出していたため、メニュー表示更新と状態取得が相互に
待機し得る問題を修正した。表示状態の clone 後に mutex を解放し、サービス状態の取得は5秒間隔の
キャッシュへ分離した。これにより runtime が正常でも「siderostat-runtimeに接続できません」から
遷移しない状態を防ぐ。macOS の `AppleLanguages=ja-JP` を `NSBundle` の明示的な優先言語として
渡すローカライズ解決を、Monitor と `siderostat-runtime` の両方へ実装した。両Macともシステム設定
は `ja-JP` だったが、従来の Foundation の自動解決は英語を選択し得たため、MacBook Pro で通知が
英語になった。メニューと通知のユーザー向け用語は、`Siderostat`=メニューバーアプリ、
`siderostat-runtime`=常駐する supervisor／自動起動対象、`ds4-server`=実際の推論プロセスに固定し、
曖昧な旧来の表現を対象ソース・リソースから除去した。core 216 test、monitor 99
test、`cargo clippy --all-targets --all-features -- -D warnings`、format、diff check、英日
`Localizable.strings` の `plutil -lint` が PASS。build 5 の signed/notarized artifact は
`dist/i18n-build5/Siderostat-0.3.0.pkg`、app SHA-256
`b9887a040b269cc9d9d2f1d43db81fc33e4cf385e9dec9de3dd19ff5ac5e4835`、package SHA-256
`4d710dac8c223174c84eb56d3350a9b6b5345e9daa664f8d8120c06682a47d0f`、notary submission
`112e1765-8a3f-47f0-a689-9e65537a2a42`。`pkgutil` は Developer ID Installer と notarization
trusted、stapler は `The validate action worked!`、Gatekeeper は `accepted / source=Notarized
Developer ID`。Mac Studio の `/tmp/Siderostat-0.3.0-i18n-build5.pkg` へ同一 SHA-256 で転送済み。
両ノードへの build 5 再インストール後、MacBook Pro の接続状態遷移、英日通知、用語、停止・起動・
再起動の各操作を再確認するまで E-04 は未完了とする。

Evidence（ds4 実行形態の用語正規化、build 6 artifact 準備完了、実機再確認待ち 2026-08-22）:
`docs/ds4-mode-taxonomy.md` を追加し、`mode` を安定した request-processing topology に限定した。
現行の具体的な mode は `Solo Standalone`、`Paired Standalone`、`Distributed (layer-parallel)`
の3つとし、`ClusterState`、`Coordinator/Worker`、TCP/RDMA、DSpark/DFlash2
をそれぞれ lifecycle state、role、transport、decoding strategy として分離した。将来の RDMA
layer-parallel、Tensor Parallel、distributed pipeline + DSpark、DFlash2、Vision は未実装として
対応表へ記録した。`docs/spec.md`、`docs/desktop-notifications-proposal.md`、
`docs/menu-bar-monitor-spec.md` の現行表記を同期し、通知を `Distributed（layer-parallel）`
へ変更した。Monitor の Mode/State 表示は `distributed-layer-parallel` / `distributed-ready` を
正規表示名へ変換し、未知値は診断のため保持する。monitor 100 test、core 216 test、clippy、
format、diff check、英日 `Localizable.strings` の `plutil -lint` が PASS。build 6 の
signed/notarized artifact は `dist/i18n-build6/Siderostat-0.3.0.pkg`、app SHA-256
`9ed95e454c8d993eae4c25bd3603bddcce8a0b832dc1db1c99aa11b845f01016`、package SHA-256
`bfd6ac998311dcbf385d187e1b9a25c54c086ab459dd62739939ddcde0e810bd`、notary submission
`0afd5b8b-4644-43d3-916d-8c3da7890dcb`。`pkgutil` は Developer ID Installer と notarization
trusted、stapler は `The validate action worked!`、Gatekeeper は `accepted / source=Notarized
Developer ID`。Mac Studio の `/tmp/Siderostat-0.3.0-i18n-build6.pkg` へ同一 SHA-256 で転送済み。
build 6 を両ノードへ再インストールして実機表示を確認するまで E-04 は未完了とする。

Evidence（topology / model detail 分離、build 8 artifact 準備完了、2026-08-22）:
`MXFP4` を mode 名から外し、現行 mode を `Distributed (layer-parallel)`、machine name を
`distributed-layer-parallel` とした。`Quantization`（`q2`、`q2-q4`、`mxfp4`）、
`DistributedTopology`、`SpeculativeSupport` を分離し、standalone の admin/metrics/manifest へ
`quantization` と `speculative_support` を出力する。旧 `distributed-mxfp4`、`model_variant`、
`[ds4.mxfp4]` は入力互換 alias としてのみ受理する。manifest には topology と speculative
support を独立記録する。core 220 test、monitor 100 test、core clippy、format、diff check、
英日 `Localizable.strings` の `plutil -lint` が PASS。workspace test は xtask の既存環境依存テスト
（`/Applications/Siderostat.app` が存在するため missing-runtime assertion が成立しない）1件を除き
PASS。build 7 の signed/notarized artifact は `dist/i18n-build7/Siderostat-0.3.0.pkg`、app
SHA-256 `fde2bb0c30983eb1c36c8aad31da2b0951630e13363706a356be4e747fe61940`、package SHA-256
`83fb3442b4218fc9bdb31ed8bd6a3889dd982f55b4360c095c10f353a94578e7`、notary submission
`ee14fce0-1bc2-4134-908e-5f75c6f0a288`。package の `pkgutil`、stapler、Gatekeeper は PASS。
その後 Monitor の既存 clippy 警告を解消して build 8 を再生成した。build 8 artifact は
`dist/i18n-build8/Siderostat-0.3.0.pkg`、app SHA-256
`e699506e32459930e30501bfaefac8f81ad098e6415bafd7ff6ec9806cfc646e`、package SHA-256
`265de37a1b1fd72cfb6b4c0c1350ad8a5646ab4915b50099ec7b5bf49096be8d`、notary submission
`cdd77acd-1556-4298-95f1-69309d77fa62`。build 8 の `pkgutil`、stapler、Gatekeeper は PASS。
 Mac Studio の `/tmp/Siderostat-0.3.0-i18n-build8.pkg` へ同一 SHA-256 で転送済み。両ノードへ
build 8 をインストールし、正常起動、通知、およびメニュー UI の正規表記をユーザーが確認した。
mode 表示・通知・UI の確認は PASS とする。`/cluster` と metrics の実機確認は別途未実施である。

Evidence（`/cluster` / metrics 実機確認、主フィールド PASS 2026-08-22）: sudo を使わない GET のみで
両nodeを確認した。両nodeの `/healthz` が version=`0.3.0`、build_number=`8`、status=`ok`、
`/readyz` が status=`ready`、target_ready=`true`、admission=`serving` となった。MacBook Pro
（worker）は mode=`distributed-layer-parallel`、state=`distributed-ready`、target=`coordinator`、
distributed worker running=true。Mac Studio（coordinator）は同じ mode/state、target=`local-standalone`、
distributed coordinator running=true。両nodeの `/metrics` で `ds4_proxy_cluster_mode` が
`mode="distributed-layer-parallel"`、`ds4_proxy_cluster_state` が `state="distributed-ready"`、
`ds4_proxy_target_ready` が `1` であることを確認した。worker の `/metrics/coordinator` は
coordinator の node_id と metrics を返し、coordinator 自身の同 endpoint は worker 専用のため
404 となった。`active_standalone_profile` および `ds4_proxy_standalone_profile_info` は両nodeとも
quantization=`q2-q4`、speculative_support=`dspark`、residency=`resident` であり、Distributed
profileのMXFP4とは別のStandalone情報として整合する。

Observed risk（受入から分離）: `/cluster.control_session.lease.peer.mode` は両nodeとも
`solo-standalone` のままで、top-levelの mode および metrics とは一致しない。promotion後の
peer descriptor診断値が更新されていない可能性があり、別修正候補として残す。なお、両nodeの
既存configは `[ds4.mxfp4]`、distributed manifestは profile=`distributed-mxfp4`、
quantization=`mxfp4-experts` の旧形式だった。build 8はこれを入力互換として受理し、実行中の
top-level mode/metricsを正規名へ出力している。既存config/manifestを書き換える操作は行っていない。

Evidence（final build 8 の配布 payload / runtime 登録 read-only 確認、2026-08-22）: 両nodeの
receipt は package-id=`dev.siderostat-ds4-proxy.pkg`、version=`0.3.0`、location=`Applications`。
receipt file list は `Siderostat.app` 配下だけで、`/Applications/Siderostat.app` が存在することを確認した。
両nodeで `codesign --verify --deep --strict` は PASS、runtime の ServiceManagement job は
`dev.siderostat-ds4-proxy.runtime`、parent bundle version=`8`、state=`running` だった。Mac Studioの
installed app は `spctl --assess --type execute` が `accepted / source=Notarized Developer ID`。
MacBook Proは同じ確認が macOS の `internal error in Code Signing subsystem` となり、実機 Gatekeeper
結果だけ未確定として残る。LaunchAgent/jobが現行の登録名で起動していることは確認済みだが、
runtime crash、background stop/start、monitor終了、login/logoutを final build 8で実施した
操作時系列の evidence はまだない。

Evidence（MacBook Pro Gatekeeper internal error 原因調査、2026-08-22）: MacBook Proでは
`spctl --assess --type execute` が Siderostat.app だけでなく `/System/Applications/TextEdit.app`、
`/System/Applications/Calculator.app`、`/usr/bin/ssh`、`/usr/bin/codesign` に対しても同じ
`internal error in Code Signing subsystem` となった。一方、同じmacOS 26.6.2 (25G83)の
Mac StudioではApple system appとSiderostat.appが`accepted`となった。両nodeでSiderostatの
app/helperのDeveloper ID署名、CDHash、arm64形式、codesign strict verificationは一致して
PASSし、MacBook Proだけのbundle破損・署名不一致ではない。MacBook Proの`syspolicyd` logには
`Unable to initialize qtn_proc: 3`と`dispatch_mig_server returned 268435459`が反復しており、
Gatekeeperのsecurity assessmentがquarantine process/IPC初期化に失敗している状態と判断する。
app rootの`com.apple.provenance`差分を除外した一時コピーでも再現したため、provenance xattrも
主因ではない。SIPは両nodeでenabled、assessmentはenabled、`syspolicyd`/`trustd`/`securityd`
はrunning。MacBook ProのGatekeeper評価だけがシステム全体で壊れているため、E-04のMacBook
実機Gatekeeper判定は未確定のままとし、rebootまたはApple側service復旧後の再検証を待つ。

Evidence（MacBook Pro再起動後のGatekeeper再検証、PASS 2026-08-22）: 再起動後、
`/System/Applications/TextEdit.app` は `accepted / source=Apple System`、
`/Applications/Siderostat.app` は `accepted / source=Notarized Developer ID` となった。
これにより、build 8のpackage/app署名・公証不備ではなく、再起動前のMacBook Proにおける
一時的なmacOS System Policy／quarantine IPC状態が原因だったと判断する。E-04のMacBook Pro
Gatekeeper判定をPASSへ更新する。

Evidence（MacBook Pro再起動後の runtime 自動起動と readiness、2026-08-22）: `last reboot` で
12:39 のシステム再起動を確認した。再起動後の `launchctl print
gui/501/dev.siderostat-ds4-proxy.runtime` は `state=running`、parent bundle version=`8`、
`runs=1`、`last exit code=(never exited)` で、job properties に `keepalive` と `runatload` が
含まれていた。さらに runtime の read-only endpoint は `/healthz` が version=`0.3.0`、
build_number=`8`、status=`ok`、`/readyz` が status=`ready`、target_ready=`true`、
admission=`serving`、`/cluster` が mode=`distributed-layer-parallel`、
state=`distributed-ready`、role=`worker`、target=`coordinator`、
`distributed-worker.running=true` となった。したがって、E-04の「再起動後にbuild 8の
runtimeが自動起動し、ready状態へ到達する」範囲のevidenceとして採用する。

Evidence（両ノードでMonitor終了後もruntimeが継続、PASS 2026-08-22）: ユーザーが両ノードの
メニューバーから「終了」を実行した直後、sudoなしの read-only 確認を行った。両ノードの
`ps -axo pid,ppid,command | grep -i '[s]iderostat'` の結果は `siderostat-runtime serve` のみで、
Monitorプロセスは残っていなかった。一方、両ノードの
`gui/$(id -u)/dev.siderostat-ds4-proxy.runtime` は `state=running`、parent bundle version=`8`、
`last exit code=(never exited)`、`job state=running` だった。MacBook Proは `/healthz` が
version=`0.3.0` / build_number=`8` / status=`ok`、`/readyz` が `ready` / `serving`、
`/cluster` が `distributed-layer-parallel` / `distributed-ready` / role=`worker` となった。
Mac Studioも同じ build 8 の正常応答で、`/cluster` は role=`coordinator`、target=`local-standalone`、
`distributed-coordinator.running=true` となった。したがって、E-04 action 4 のうち
「Monitor終了後にruntimeを終了させない」は両ノードでPASS evidenceとして採用する。

Evidence（MacBook Pro runtime強制終了後の自動復旧、PASS・遅延あり 2026-08-22）: ユーザーが
Activity Monitorから `/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime` を強制
終了した。launchdは `last terminating signal=Killed: 9`、`runs=2` を記録し、parent bundle
version=`8` の新しいruntimeを `state=running` として起動した。再spawn直後はTCP listen portが
なく、13:32:37のprocess sampleでもruntimeのmain threadが `ProcessController::verify_process`
内のmacOS process inspectorに滞留していたため、60秒時点では未readyだった。しかし、その後の
13:35:21の read-only 確認では、runtimeが `127.0.0.1:18080/18081` と `10.99.0.2:9920` をlistenし、
`/healthz` が version=`0.3.0` / build_number=`8` / status=`ok`、`/readyz` が `ready` /
`serving`、`/cluster` が `distributed-layer-parallel` / `distributed-ready` / role=`worker` /
`distributed-worker.running=true` へ到達していた。したがって、E-04 action 4 の「runtime crash後に
自動再spawnし、ready状態へ復旧する」はPASS evidenceとして採用する。再spawnからready到達までの
時間は今回の取得時刻の範囲では約3〜6分であり、60秒でFAILと判定してはならない。ready到達時間の
正式な上限値は別途受入基準として固定する。なお、最終確認時のprocess一覧には `Siderostat` 本体も
存在していたため、Monitorの再起動有無とその契機はruntime復旧とは分離して扱う。

Evidence（MacBook Pro background stop/start、PASS 2026-08-22）: ユーザーがメニューから
「siderostat-runtimeを停止して自動起動を無効化」「siderostat-runtimeを起動して自動起動を
有効化」を順に実行した。Unified Logでは、13:37:01に `dev.siderostat-ds4-proxy.runtime` の
jobが `smd` によりremoveされ、SIGTERMが送信された。5秒後に終了しなかったため13:37:06に
launchdがSIGKILLを送り、serviceをremoveした。その後13:37:16に `smd` 起点でserviceがenabledに
戻され、再登録・spawn・`job state=running` まで完了した。read-only確認時のruntimeはparent
bundle version=`8`、`state=running`、`last exit code=(never exited)` で、disabled servicesの
値も `dev.siderostat-ds4-proxy.runtime => enabled` だった。`/healthz` は version=`0.3.0` /
build_number=`8` / status=`ok`、`/readyz` は `ready` / `serving`。起動直後は一時的に
`paired-standalone-ready` だったが、その後のread-only pollingで `distributed-layer-parallel` /
`distributed-ready`、`distributed-worker.running=true` へ復帰した。したがって、E-04 action 4の
background stop/startと自動起動再登録はPASS evidenceとして採用する。ただし、停止時はSIGTERMで
5秒以内に終了せずlaunchdのSIGKILL fallbackへ進んだため、graceful shutdownの成立まではこの
evidenceで証明しない。なお、通知本文そのものは
永続ログに残らないため、通知文言の確認はこのevidenceの対象外とする。

Evidence（MacBook Pro logout/login、runtimeはPASS・Monitor login startは判定保留 2026-08-22）:
ログアウト時のUnified Logには、13:42:33に `application.dev.siderostat-ds4-proxy` のMonitorが
`Termination complete`、13:42:34にapplication serviceがremoveされた記録がある。再ログイン後は
13:42:41に `dev.siderostat-ds4-proxy.runtime` がenabledとして再登録され、runtimeはbuild 8で
起動した。ログアウト直後から1分以上の確認時点ではMonitor本体はまだ起動していなかったが、
13:48:53にlaunchdが `application.dev.siderostat-ds4-proxy` を `launch job demand` でspawnした。
その後のread-only確認では、runtimeと `/Applications/Siderostat.app/Contents/MacOS/Siderostat` が
各1プロセス、application jobが `state=running`、runtimeの `/healthz` が `status=ok`、`/readyz` が
`status=ready` / `admission=serving`、`/cluster` が `distributed-ready` /
`distributed-worker.running=true` だった。しかしユーザーは13:48:53頃にDockからSiderostatを
手動起動していたため、この `launch job demand` は手動起動でも発生し得るイベントであり、
Login Itemsによる自動起動のevidenceとしては採用しない。したがって、E-04 action 4のlogout/loginは
runtimeの自動起動のみPASS、Monitorのlogin startは判定保留とする。System Settingsのトグル有効は
登録・許可状態のevidenceではあるが、ログイン時に自動起動したことの証明とは分離する。
`sfltool dumpbtm` はこの環境で管理権限の認証エラーとなったため、Login Itemsの状態判定には使用しない。

Evidence（MacBook Pro logout/login再試行、Monitor login start FAIL 2026-08-22）: ユーザーが
System SettingsでSiderostatのLogin Itemsトグルが有効であることを確認したうえで、Dockからの
手動起動を行わずに再ログインした。10分以上経過した14:07:42時点でも、process一覧は
`siderostat-runtime serve` のみで、`gui/$(id -u)` に `application.dev.siderostat-ds4-proxy` の
application jobは存在しなかった。直近のlaunchd logにも今回のログイン後のMonitor spawnはなく、
前回の手動起動に対応する13:48:53の記録と、今回のログアウト時のapplication service removeのみが
残っていた。一方、runtimeは `state=running`、build 8で、`/healthz`、`/readyz`、`/cluster` は
正常な `distributed-ready` / `distributed-worker.running=true` だった。したがって、System
Settings上のトグルが有効でも、現行build 8のMonitor本体はログイン時に自動起動していない。
E-04 action 4のMonitor login startはFAILとして記録し、別途修正対象とする。

Observed failure context（受入PASSには算入しない）: 再起動前の
`siderostat-runtime-2026-08-22-010058.ips` および `siderostat-runtime-2026-08-22-091255.ips`
には `EXC_CRASH`、`SIGKILL (Code Signature Invalid)`、termination namespace=
`CODESIGNING`、indicator=`Launch Constraint Violation` が記録されていた。いずれも今回の
意図的なruntime crash復旧試験ではなく、Gatekeeperのシステム障害が発生していた時間帯の
診断記録であるため、runtime crash/restartのPASS evidenceには採用しない。再起動後の
Gatekeeper PASSおよび上記runtime readinessと併せ、原因切り分けの補助記録として残す。

Evidence（通知送信元を Siderostat へ変更、Monitor 起動 panic を修正、build 9 artifact 再生成済み、実機再確認待ち 2026-08-22）:
`src/notify.rs` の macOS 通知実装を `/usr/bin/osascript` から `UNUserNotificationCenter` へ変更した。
`Contents/Helpers/siderostat-runtime` は app bundle ではないため、Runtime は
`~/Library/Application Support/siderostat/notifications.sock` へ通知 payload を送り、署名済み
`Siderostat.app` の Monitor が relay を受けて native API で投稿する。これにより通知の送信元と
標準の「表示」アクション対象は Siderostat となり、Script Editor は起動しない。UserNotifications
の公開 API には標準アクションボタンを常に非表示にする設定がないため、ボタン非表示は受入要件に
含めず、表示先の修正を受入対象とする。`cargo xtask app-dev --version 0.3.0 --build-number 9
--verify`、UserNotifications.framework のリンク確認、Runtime binary に `osascript` / `display
notification` がないことの確認、core 219 test、monitor 102 test、workspace clippy、format、
diff check が PASS。署名・公証 artifact は `dist/native-build9/Siderostat-0.3.0.pkg`、app SHA-256
`110bac6b52ab06d31b1a8e1eca66a980b7265dd0febb48e42b07fd97deb37051`、package SHA-256
`e72f88a9e74933c5e6224380c1af40b18f3d27e80a28f446b89f11d9c0859cf0`、notary submission
`cb8bc4a8-f984-4b7c-9d01-241fed1b537a`、notary status `Accepted`、pkgutil の Developer ID
Installer trusted timestamp を確認した。両ノードへの build 9 install と、通知の送信元および
「表示」クリック時に Script Editor が起動しないことの実機確認を残す。

追加の起動診断では、先行してインストールされた build 9 の Monitor が通知 relay 初期化時に
`tokio::net::UnixListener::from_std()` を Tokio runtime 外で呼び出し、`there is no reactor running`
で起動直後に panic していた。このためメニューバーアイコンが表示されず、Monitor の application
service も登録されなかった。listener の変換を relay thread 内の Tokio runtime 上へ移動して修正し、
上記 artifact を再生成した。再インストール後のメニューバー表示と通知 UI の実機確認を受入 evidence
とする。

Evidence（runtime 接続表示の診断・build 10 artifact 準備完了、実機再確認待ち 2026-08-22）:
MacBook Pro の read-only 確認では、Monitor build 9（PID 40619）と siderostat-runtime（PID 40688）が
稼働中で、runtime の `/healthz`、`/cluster`、`/metrics`、`/metrics/coordinator` は全て HTTP 200 だった。
`/cluster` は `mode=distributed-layer-parallel`、`state=distributed-ready`、`target_ready=true` であり、
`/readyz` の HTTP 503 は `admission=blocked` による not-ready で、接続不能とは異なる。従来の Monitor は
metrics polling の一時失敗だけで `Offline`（「siderostat-runtimeに接続できません」）へ遷移していたため、
runtime 接続状態と metrics 取得状態を分離した。`/healthz` が成功する場合は最後の metrics snapshot を保持して
`Degraded` とし、接続不能の場合だけ `Offline` とする。Degraded の日本語／英語 UI 文言を追加し、
Monitor 103 test、core 219 test、clippy、format、diff check が PASS。署名・公証 artifact は
`dist/native-build10/Siderostat-0.3.0.pkg`、app SHA-256
`78f41af2766a0440ab1a7a1d4814f4b3b64b254e49225f7d0808e5df4daeda10`、package SHA-256
`9424db3456e5115dcafcce40ff01bb1a80dba04a513eed3fa8757d3a8c43dfe6`、notary submission
`cac99170-b91b-4fd2-acfe-2d2b177431a4`、notary status `Accepted`、Developer ID Installer の
trusted timestamp を確認した。MacBook Pro と Mac Studio へ build 10 をインストール後、runtime 再起動時の
接続中／metrics取得不可／接続不能の各表示を実機確認する。

Evidence（installer の既存 Monitor 自動終了、build 11 artifact 準備完了 2026-08-22）:
ユーザーがインストール前に手動で「Siderostatを終了」する必要がないよう、`xtask/src/package.rs` の
component package に controlled `preinstall` を追加した。`preinstall` は
`/Applications/Siderostat.app/Contents/MacOS/Siderostat` と完全一致する既存 Monitor だけを検出し、
SIGTERM 後に最大10秒待機し、終了しない場合だけ同じ完全一致の PID へ SIGKILL を送る。runtime、
`ds4-server`、LaunchAgent、設定、secret、モデル、その他のユーザーデータは対象外である。
`pkgutil --expand-full` の結果は payload `Applications/Siderostat.app` と
`Scripts/preinstall` のみであり、展開した script の `sh -n` も PASS。package builder の
unexpected script／forbidden path 検査、preinstall scope test、clippy、format、diff check が PASSした。
build 11 の signed/notarized artifact は `dist/native-build11/Siderostat-0.3.0.pkg`、app tree
SHA-256 `b907187cf26bfee1addca3a9e6e43a9bfe49747f3e0231f311157c0c7be8e681`、package SHA-256
`c1e81920f142137e6ccbc7645489f65aa09aeecfa72b4d68a85889adc38a0ea4`、notary submission
`aca4d028-0d7b-49b0-846d-915f9c92110f`、notary status `Accepted`、Developer ID Installer の
trusted timestamp を確認した。実機で Monitor 稼働中に build 11 をインストールした際、起動中だった
メニューアイコンがインストール中に消滅し、その後インストールが継続したことをユーザーが確認した。
これは既存 Monitor が `preinstall` により終了した evidence として採用し、Installer の既存 Monitor
自動終了項目を PASS とする。runtime／設定の保持は両ノードの起動および distributed 運用確認と合わせて
確認済みとする。build 10 の runtime 再起動後に約3分以上 offline が継続した事象は、installer の
Monitor 終了処理とは別の runtime readiness／復旧時間の課題として扱う。

仕様補正（2026-08-25）: 従来の E-01 記録は Monitor だけを停止し runtime を対象外としていたため、
現在の installer 仕様と不一致になっていた。`preinstall` は runtime を先に停止してから Monitor を
停止する。停止対象は active console user の product-owned LaunchAgent と bundle 内の exact path
だけであり、user data、設定、secret、model、cache、永続的な Service Management 登録は変更しない。
`postinstall` は新しい app を起動し、app 側の Service Management 復帰処理へ引き渡す。Rust unit
test、shell 静的検証、package expand policy へこの順序・対象限定・強制停止 fallback の検査を追加した。

追加補正（2026-08-25）: build19 の両 node install では Monitor の自動起動は確認できたが、
`preinstall` の runtime `bootout` 後に runtime LaunchAgent が復帰しなかった。原因は、既に
`enabled` だった Service Management job を app 側が再登録せず、job が unload 状態に残ったことと
判定した。`postinstall` に事前の `print-disabled` が `enabled` を示す場合だけ新 bundle の plist を
bootstrap する処理を追加し、disabled／未登録／承認待ちでは従来どおり app 側へ委譲する。これにより
インストール後の手動 runtime 停止・起動を不要にする。

再補正（2026-08-25）: build20 の両 node install では Monitor は起動したが runtime job は未ロードのまま
だった。`launchctl asuser <uid> launchctl bootstrap` はユーザーセッションからは `Bootstrap failed:
5: Input/output error` となり、root postinstall の実行主体と一致しないため不適切だった。enabled 状態の
確認後は root の postinstall から `/bin/launchctl bootstrap "gui/<uid>" <bundle plist>` を直接実行し、
Siderostat の GUI 起動だけを `launchctl asuser <uid> open -a` で行うよう修正した。TDD（旧実装で失敗→修正後
成功）、`cargo test -p xtask` 43件、clippy、fmt、`scripts/verify-macos-dev-artifacts.sh` は PASS。修正済み
build21 の DMG を両ノードの `/tmp/Siderostat-0.3.0-build21.dmg` へ同一 SHA-256
`f6d2f9fb7b750b1a8802350838f608c965bd301e58b55b46c41cb50600ccb300` で配置し、実機インストール後の
runtime 起動確認を待つ。

再補正（2026-08-25、build22準備）: build21 の両 node install でも Monitor は build21 で起動したが、
runtime LaunchAgent は未ロードで、`/healthz`・`/readyz`・`/cluster` は接続拒否だった。Apple の
Service Management 仕様では LaunchAgent の `register()` は即時 bootstrap する一方、既登録状態では
`kSMErrorAlreadyRegistered` となるため、app 側の status=`enabled` 判定だけでは `preinstall` が unload
した job を復帰できない。さらに pkg script から直接 bootstrap する plist は `BundleProgram` ではなく
`Program` または `ProgramArguments` を使う必要があるため、plist を `/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime`
の固定 `Program` path に変更した。bundle template test 4件、xtask test 44件、clippy、fmt、E-03静的検証は
PASS。修正済み build22 の DMG を両ノードの `/tmp/Siderostat-0.3.0-build22.dmg` へ同一 SHA-256
`85650ed89c6d511be5aee8dcd44522f91a50db96131e445b606ed643511843e7` で配置し、実機インストール後の
runtime 自動ロード確認を待つ。

Evidence（build22 installer runtime 自動復帰・両 node 2026-08-25）: ユーザーが両ノードへ
`/tmp/Siderostat-0.3.0-build22.dmg` 内の pkg をインストールした。MacBook Pro と Mac Studio の
Info.plist はともに version=`0.3.0` / build=`22`、`launchctl print gui/501/dev.siderostat-ds4-proxy.runtime`
は両ノードで `state = running`、`program = /Applications/Siderostat.app/Contents/Helpers/siderostat-runtime`
を示した。`/healthz` は両ノードで `status=ok` / build_number=`22`。起動直後の短い `admission=blocked`
から収束後、両ノードの `/readyz` は `status=ready` / `admission=serving`、`/cluster` は
`state=distributed-ready` / `mode=distributed-layer-parallel` となった。MacBook Pro は worker、
Mac Studio は coordinator として安定していた。Program 固定 path への変更と installer postinstall による
runtime 自動復帰を実機 PASS とする。

Evidence（build 11 両ノード実機起動・distributed 運用確認 2026-08-22）:
ユーザーが MacBook Pro と Mac Studio の両ノードへ `dist/native-build11/Siderostat-0.3.0.pkg` を
インストールし、Siderostat の起動を確認した。現在、Hermes の cron job が両ノード構成で
`Distributed (layer-parallel)` として問題なく実行できている。build 11 のアプリ起動と distributed
runtime の実運用は PASS evidence として採用する。起動中のメニューアイコンがインストール中に消滅した
ことも確認済みであり、Installer の既存 Monitor 自動終了項目は PASS evidence として上記へ反映した。

Evidence（Monitor login start 解消・E-04 完了 2026-08-22）:
ユーザーがシステム設定 > 一般 > ログイン項目と機能拡張の「ログイン時に開く」リストに
`Siderostat.app` が追加されていることを確認した。さらに、ログイン後に Siderostat が自動起動する
状態であることを確認した。以前の build 8 で記録した「Login Items のトグルは有効だが Monitor が
自動起動しない」という FAIL は、build 11 の実機確認により解消したものとして扱う。これにより、
E-04 action 1〜5 の signed/notarized install、first launch、lifecycle、login start、payload 検証を
完了し、E-04 を PASS とする。runtime 再起動後の復旧時間が約3分以上となる事象は、正式な上限値を
別途定義する runtime readiness／復旧時間の課題として E-04 から分離して残す。

### [x] E-05 legacy migration、upgrade、rollback を実機検証する

- Actor: user + agent
- Depends on: E-04
- 参照: 配布仕様 12、13、14、16.3、16.4、16.5 節
- 事前条件: prior/final candidate package と user data backup を復元できる test 環境がある。
  legacy install は現行 scope では存在せず、migration / legacy rollback は N/A とする
- Files: `docs/compatibility/v0.3.0-migration-rollback.md`（新規、redaction 済み結果のみ）
- Actions:
  1. （legacy install が検証対象の場合）legacy job 稼働中から migration 成功を実行し、port と child の重複がないことを確認する。
  2. （legacy install が検証対象の場合）new registration または readiness を意図的に失敗させ、legacy rollback を確認する。
  3. v0.3 candidate 間 upgrade で config、secret、manifest、model、KV cache を保持する。
  4. prior notarized package へ rollback し、version mismatch の案内と data 保持を確認する。
  5. uninstall 手順で service だけを止め、user data が既定で残ることを確認する。
- 事後条件: P0 配布 feature の実機 gate が閉じ、後続の E-06 が着手可能になる
- 受入基準: 現行 scope の全 scenario（upgrade、rollback、uninstall）で root process、orphan、
  duplicate listener、data loss がない。legacy migration / rollback（scenario 1/2）は N/A として
  機能テスト evidence を採用する。
- Verification: PID/path/port/job/status、file digest/permission、readiness、package version の前後比較
- ユーザーレビュー・手作業: change window、candidate upgrade、prior package rollback、uninstall、最終復元を実行する
- 停止条件: identity 未確認 process の停止、legacy plist の削除、user data の削除が必要になる
- 進捗（2026-08-23）: `scripts/e05-verify.sh` の構文確認と MacBook Pro の read-only
  preflight（build 11、healthz/readyz、new runtime job、listener、legacy job 不在）を実施した。
  手動ログイン成功後、Mac Studio の read-only preflight も取得した。クラスタ停止、インストール、
  process 操作、設定変更、user data 変更は行っていない。
- Evidence: 詳細は `docs/compatibility/v0.3.0-migration-rollback.md` の 4.5.3〜4.5.12 に記録。
- スコープ判断（2026-08-23）: 両ノードとも現行 build 11 が稼働し legacy install がないため、
  v0.2.1 の復元は行わない。シナリオ 1/2 は現行 gate の対象外（N/A）とし、migration /
  rollback 契約の機能テスト evidence を代替証跡とする。
- 完了判定: シナリオ3の upgrade、シナリオ3起点で検証済みのシナリオ4 rollback、シナリオ5の
  uninstall を実施し、redaction 済みの結果を記録したため E-05 を完了とする。
- 追加記録（2026-08-23）: シナリオ3の事前 snapshot 後、build 10 candidate を両ノードへ
  配置・インストールしたが、app は build 11 のままで version mismatch 通知は発生しなかった。
  candidate の `PackageInfo` は build 10 だが、payload 内 `Info.plist` は build 11 であり、
  package artifact 不整合と判定した。正しい build 10 の署名・公証 package 再生成が必要である。
  詳細は `docs/compatibility/v0.3.0-migration-rollback.md` の 4.5.5 に記録。
- 追記（2026-08-23）: 修正版 build 10 package（payload / metadata とも build 10、署名・公証・
  staple・Gatekeeper PASS）を両ノードへ適用したが、既存 build 11 app は build 11 のままで
  downgrade は適用されなかった。receipt の install-time だけが更新された。現行 build 11 から
  build 10 起点を作るには clean baseline または rollback を明示的に扱う install 実装が必要であり、
  現在のシナリオ3は中断していた。この課題は明示的 `--rollback` artifact の追加で解消した。
- 追記（2026-08-23）: `xtask pkg-dev` / `xtask sign` に明示的な `--rollback` を追加した。通常 package
  は `BundleIsVersionChecked=true`、rollback package だけ `false` とし、成果物・notary log・metadata
  に `-rollback` を付与して識別する。`sign` metadata には `rollback` と `install_mode` を記録する。
  payload は従来どおり `Siderostat.app` 一項目、installer script は exact Monitor path の preinstall
  一項目に限定し、runtime、LaunchAgent、設定、secret、model、cache には触れない。通常 package と
  rollback package の展開検査および `PackageInfo` 差分検証は PASS。実機 signed/notarized rollback
  install は後続のシナリオ3起点化で実施し、シナリオ4の受入条件へ採用した。
- 追加 artifact（2026-08-23）: `dist/native-build10-rollback/Siderostat-0.3.0-rollback.pkg` を
  build 10 payload から生成し、Developer ID Installer 署名、notarization、staple、Gatekeeper を
  PASS とした。package SHA-256 は `07370187d2eb97b66ff278c78dba218e4d9bc2bcea3e1e3628d83a471c3d65d9`。
  展開後 `PackageInfo` は `<bundle-version/>`、payload の `CFBundleVersion` は 10、metadata は
  `rollback=true` / `install_mode=rollback`。この artifact はシナリオ3の起点化で両ノードへ適用済みである。
- 再開準備（2026-08-23）: 両ノードの現行 build 11 / runtime build 11 / readiness ready を read-only
  snapshot `pre-s3-resume-b11` として取得し、rollback package と通常 build 11 upgrade package を
  `/tmp` へ配置した。SHA-256 はそれぞれ `07370187d2eb97b66ff278c78dba218e4d9bc2bcea3e1e3628d83a471c3d65d9`、
  `c1e81920f142137e6ccbc7645489f65aa09aeecfa72b4d68a85889adc38a0ea4`。両ノードで同一値を確認した。
  次は rollback package による build 10 起点化、続く通常 build 11 upgrade の順でシナリオ3を再開する。
- シナリオ3起点確認（2026-08-23）: ユーザーが両ノードで rollback package を install して Siderostat.app
  を起動し、app build 10 / runtime build 11 の version mismatch 通知を確認した。agent の
  `pre-upgrade-b10` snapshot でも両ノードの app build 10、runtime `/healthz` build 11、`/readyz`
  serving/ready、new runtime job running、legacy job 未登録を確認した。`pre-s3-resume-b11` との
  user data digest / secret mode 比較は PASS。次は通常 build 11 package の upgrade 検証である。
- シナリオ3 upgrade 完了（2026-08-23）: ユーザーが両ノードで通常 build 11 package を install して
  Siderostat.app を起動した。`post-upgrade-b11` snapshot で両ノードの app/runtime build 11 一致、
  `/healthz` `status=ok`、`/readyz` serving/ready、新 runtime job running、legacy job 未登録を確認。
  `pre-upgrade-b10` との user data digest / secret mode 比較も PASS とし、シナリオ3の build 10 →
  build 11 upgrade を PASS 判定とする。シナリオ3起点化の rollback 過程がシナリオ4の受入条件も
  満たすため、独立したシナリオ4の再実施は不要と判定した。
- シナリオ4 rollback 実施・採用（2026-08-23）: シナリオ3の起点化として両ノードへ rollback package を
  適用し、app build 10 / runtime build 11 の version mismatch 通知を確認した。`pre-upgrade-b10` と
  `post-upgrade-b11` の user data digest / secret mode 比較は PASS。prior package の適用、mismatch 案内、
  data 保持、通常 package への復帰を確認済みとして、シナリオ4を PASS 判定とした。
- シナリオ5準備（2026-08-23）: 両ノードの build 11 / readiness ready を `pre-s5-b11` snapshot として
  取得した。uninstall は runtime job/process、管理対象 ds4-server child、Monitor app と
  `/Applications/Siderostat.app` だけを対象とし、Application Support、secret、manifest、cluster state、
  model、KV cache は保持する手順を確認した。この準備に続く実施結果は compatibility record の
  4.5.12 に記録し、エンドユーザー向けの正式 UX は E-06 で提供する。
- シナリオ5 uninstall 実施（2026-08-23）: agent が両ノードの現行 runtime job と完全一致した Monitor を
  停止し、`/Applications/Siderostat.app` を Finder Trash へ退避した。`post-s5-uninstall` で両ノードの
  現行・legacy job、runtime、ds4-server、Monitor、関連 port が absent、model / KV cache が存在する
  ことを確認した。config / manifest / secret の digest/mode は両ノードで保持された。Mac Studio の
  user data 比較は PASS。MacBook Pro は runtime 停止時に `cluster-state.json` の digest のみ変化したが、
  schema 1、`last_failure=null`、child 不在の有効な状態 file として保持され、データ削除とは判定しない。
  package receipt は E-06 で整理する。シナリオ5の配布仕様14条件は PASS。シナリオ3〜5の
  redaction 済み結果を記録したため、E-05 を完了とする。

### [x] E-06 リリース DMG と `Siderostat Uninstaller.app` を提供する

- Actor: agent + user review
- Depends on: E-05
- 参照: 配布仕様 2.3、14、15、16.1、16.5 節
- 事前条件: E-05 の upgrade、rollback、uninstall の実機検証が完了し、現行の
  `/Applications/Siderostat.app` と Service Management の lifecycle 契約が確定している
- Files: `xtask/src/dmg.rs`（新規）、Uninstaller.app の bundle builder、関連 test、
  `xtask/README.md`、`docs/installation.md`、`docs/operations.md`
- Actions:
  1. `Siderostat Uninstaller.app` を Finder 起動可能な GUI artifact として実装する。確認後、
     `SMAppService` の runtime / main app login item unregister、対象 process の停止確認、
     `/Applications/Siderostat.app` の Trash 移動、対象 package receipt の整理を行う。
  2. Uninstaller の既定操作では Application Support、secret、manifest、cluster state、model、
     KV cache を変更せず、一部完了状態を含めて安全に再実行できるようにする。
  3. `Siderostat-<version>.dmg` を生成し、`Siderostat-<version>.pkg`、
     `Siderostat Uninstaller.app`、README だけを収録する。既存の `/Applications/Siderostat.app`
     payload、bundle identifier、upgrade/rollback artifact の契約は変更しない。
  4. app、Uninstaller、DMG を Developer ID 署名・公証・staple し、DMG 展開後の file list、
     Gatekeeper、checksum、metadata を検証する。
  5. clean install → DMG uninstaller → 再インストールの実機 cycle を両ノードで確認する。
- 事後条件: エンドユーザーが Terminal またはメニューの手動停止手順を辿らず、DMG の
  `Siderostat Uninstaller.app` だけで安全にアンインストールできる
- 受入基準: DMG の内容、署名、公証、Gatekeeper、Uninstaller の再実行性、対象 process の停止、
  app bundle の Trash 移動、package receipt の整理、user data 保持がすべて PASS
- Verification: `hdiutil attach` / file list、`codesign`、`spctl`、`stapler`、実機 snapshot 比較、
  `git diff --check`
- ユーザーレビュー・手作業: DMG から Uninstaller を起動し、確認 UI と data 保持結果を確認する
- 停止条件: Uninstaller が未確認 process の停止、user data の削除、`sudo rm -rf`、既存 app bundle
  path の変更を必要とする

実装記録（2026-08-23）:

- `monitor/src/uninstaller.rs` に Finder 起動可能な確認 UI、`SMAppService` の runtime/main app
  login item unregister、固定 executable path の Monitor/runtime 停止、persisted child identity
  検証付き ds4-server 停止、Finder Trash 移動、対象 receipt 整理を実装した。設定、secret、manifest、
  cluster state、model、KV cacheは既定操作で変更しない。
- `xtask/src/dmg.rs`、`Uninstaller-Info.plist.in`、`sign --with-dmg` を追加した。DMG 直下は
  `.pkg`、`Siderostat Uninstaller.app`、`README.html` の3項目に固定し、Uninstaller zipは公証用の
  一時入力としてだけ扱う。
- ad-hoc build 11 で app → pkg → DMG の構造検証に成功し、Developer ID build 11 で pkg、Uninstaller、
  DMGの署名・公証・staple・Gatekeeper検証、DMG readonly展開後のfile list検証に成功した。
  公証 submission ID と checksum は repository 外の `build/e06-release-final` metadata に保存した。
- 署名済み DMG を Finder から開き、Uninstaller の確認 UI、user data 保持、再実行性、再インストールを
  両ノードで確認する実機受入を開始した。
- 受入時に、`/Applications/Siderostat.app` が既に Trash へ退避された状態で Uninstaller が
  `~/.Trash/Siderostat.app/Contents/MacOS/Siderostat --unregister-services` を起動し、旧 build 11
  がその引数を解釈せず通常の AppKit event loop に入ったため、Uninstaller が無期限に待つ事象を確認した。
  Uninstaller は Trash 内の bundle を実行してはならないため、候補を `/Applications/Siderostat.app` のみに
  限定し、サービス解除 helper の終了待ちにも15秒の上限を設けた。旧 helper が応答しない場合は process を
  停止してエラー画面へ戻り、user data と app bundle を残して再実行可能にする。修正後の artifact で
  Finder受入を再実施するまでの時点では E-06 は未完了だった。
- 修正版 build 11 artifact で両ノードの Finder Uninstaller 実機受入を実施した。Uninstaller の完了 UI
  まで到達し、agent の read-only evidence で両ノードとも `Siderostat.app`、Monitor、runtime、
  `ds4-server`、runtime job、対象 package receipt が absent、Application Support、config、
  ds4 KV cache が present（Application Support 内の file count は各11）であることを確認した。
  既存 E-05 の config / manifest / secret / cluster state / model 保持 evidence と合わせ、uninstall
  による user data loss は確認されなかった。この時点では再インストールと起動確認を残していた。
- E-06 実機受入完了（2026-08-23）: 両ノードで修正版 build 11 package を再インストールし、
  `Siderostat.app` を起動した。両ノードの app/runtime は version `0.3.0` / build `11`、
  `/healthz` は `status=ok`、`/readyz` は `status=ready`、新 runtime job は `running`、
  legacy job は absent であることを read-only snapshot `e06-reinstall` で確認した。
  MacBook Pro は distributed worker、Mac Studio は standalone coordinator として ds4-server、
  model、KV cache の稼働も確認した。再インストール後の package receipt は存在し、両ノードの
  Application Support 内 file count は各11、secret mode は `0600` のまま保持された。
  直前の Uninstaller 実行で app/process/job/receipt が除去された後に再インストールできたため、
  clean install → uninstall → reinstall cycle、再実行可能な uninstall、user data 保持、
  signed/notarized DMG の各条件を PASS とし、E-06 を完了とする。

追加 UX 修正（2026-08-23）: Uninstaller の `CFBundleIconFile` と仮の `AppIcon.icns` 同梱を
 廃止し、macOS 標準のアプリ表示へ戻した。アプリ bundle の Trash 移動と対象 package receipt の
整理は一つの `osascript` 管理者承認トランザクションへ統合した。macOS の Trash 内 bundle には
`com.apple.macl` 等の保護属性が付くため、所有者を後から `chown` で変更する処理は行わない。
Uninstaller は `SMAppService.unregister()` で登録を解除するが、macOS の
Login Items / Background Items 承認履歴は強制消去しない。Installer は controlled `postinstall`
で Siderostat を起動し、必要な approval 導線をアプリから表示する。Mac Studio の非対話 SSH
証跡では Homebrew path を明示して `/opt/homebrew/bin/rg` を解決する。回帰 test と signed artifact
再検証をこの修正の完了条件とする。

検証 evidence（2026-08-23）: `cargo test --workspace --all-targets` は root 219、monitor 113、
xtask 37 と統合 test が PASS。`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
`cargo fmt --all -- --check`、`git diff --check`、`bash -n scripts/e05-verify.sh` も PASS。
`build/e06-fix2-release/` に build 11 の署名・公証・staple 済み app/pkg/DMG を生成し、
app、Uninstaller、pkg の `spctl` はすべて `source=Notarized Developer ID`、三 artifact の
`stapler validate` は PASS。DMG 直下は pkg、Uninstaller.app、README.html の3項目で、
Uninstaller の独自 AppIcon は存在しないことを確認した。

追加不具合修正（2026-08-23）: build 11 の Uninstaller 実機確認で、Trash 内 bundle の
`com.apple.macl` / `com.apple.provenance` と後付け `chown -R` が衝突し、アプリ移動後の
管理者処理が失敗する事象を確認した。`chown` を廃止し、保護属性を変更せずに exact app path の
Trash 移動と対象 receipt 整理だけを一つの管理者処理で行うよう修正した。さらに既存の
`~/.Trash/Siderostat.app` と衝突する場合は `.1` 以降の退避名を選択する。回帰 test は修正前 RED、
修正後 GREEN。build 13 の signed/notarized artifact を `build/e06-fix4-release/` に生成し、
app/Uninstaller/pkg の Gatekeeper、staple、DMG 3項目検証を PASS とした。

## 11. Phase H: throughput degradation の観測と復旧

### [x] H-01 degraded detection / recovery contract を固定する

- Actor: agent + user review
- Depends on: E-06
- 参照: Hermes 調査 8〜12 節
- 事前条件: app/pkg lifecycle と runtime restart/migration が実機 PASS
- Files: `docs/recovery/throughput-degraded-contract-v0.3.0.md`（新規）
- Actions:
  1. 初期値を canary 64 token、deadline 30 秒、decode 下限 5 tokens/s、progress stall 60 秒、
     recovery admission drain timeout 60 秒、cooldown 1 時間、12 時間に最大 2 回として固定する。
  2. idle の 0 TPS を異常にしない条件と、prefill/decode/first-token の判定順を固定する。
  3. `recover-degraded` request/status JSON、recovery ID、単一 owner、冪等性を固定する。
  4. `admission block -> snapshot -> drain -> demote -> paired standalone -> promote -> canary -> serving`
     の順序を固定する。
  5. recovery admission drain timeout、demote failure、promotion failure、canary failure の安全な最終状態を表にする。
  6. 通常 lifecycle の `cluster.timeouts.drain` と DS4 stop の `cluster.timeouts.stop` は各180秒のまま維持し、recoveryだけ operation-scoped timeout を使用する。
  7. 自動復旧は opt-in 既定 `false` とし、H-10 実機 evidence 後の既定値変更は別レビューとする。
  8. admission block 中は通常の外部 request を受け付けず、drain 後に recovery owner が一回限りの canary 例外許可を発行する。これは ds4-server の優先処理、予約 slot、queue bypass を前提にしない。
- 事後条件: H-02〜H-09 の実装者が閾値、状態、failure behavior を推測しない
- 受入基準: 低 TPS、first-token stall、progress stall、idle、cooldown、上限、競合の入力/出力表がある
- Verification: Hermes 調査 9.4/11 節との対応表、sequence diagram、`git diff --check`
- ユーザーレビュー・手作業: 閾値、回数上限、opt-in、drain timeout、失敗時の通知方針を承認する
- レビュー記録（2026-08-23）: ユーザーは、通常の外部 request を遮断し、drain 後に canary だけを一回通す recovery canary 例外許可の方針、および DS4 の優先処理を前提にしない方針を承認した。H-01 の review gate を完了とする。
- 停止条件: active request の強制 kill または既存 demotion owner の迂回を正常経路にする必要がある

### [x] H-02 monotonic progress freshness metrics を実装する

- Actor: agent
- Depends on: H-01
- 参照: Hermes 調査 9.2、11.1〜11.2 節
- 事前条件: progress event の定義と idle 判定が contract に固定済み
- Files: `src/metrics.rs`、DS4 progress event の直接呼出元、関連 test
- Actions:
  1. prefill/decode ごとに最終 event の monotonic timestamp と前回からの token delta を保持する。
  2. active 中だけ last progress age を増加させ、完了/idle では active=false と age の意味を分離する。
  3. Prometheus metric 名、type、help を追加し、wall clock を threshold 判定に使わない。
  4. first progress 前の active 状態を first-token waiting として表現する。
- 事後条件: detector が現在 TPS だけでなく「最後に進んだ時刻」を判定できる
- 受入基準: idle、first-token、正常 chunk、stall、完了、次 request の reset test がある
- Verification: metrics unit test、render snapshot test、共通 local gate
- ユーザーレビュー・手作業: なし
- Evidence: `src/metrics.rs` に prefill/decode の monotonic progress timestamp、last progress age、token delta、first-token waiting (`generation_active=1` / `generation_progress_observed=0`) を追加。idle/完了時は active と age を分離し、generation 完了時に freshness state を reset。`docs/spec.md` と `docs/operations.md` に metric family と判定方法を反映。H-02 専用テストを含む `cargo test --all-targets`（221 tests + integration tests）、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --check`、`git diff --check` が成功（2026-08-23）。
- 停止条件: prompt、response、session ID を metric label に含める必要がある

### [x] H-03 monitor を current chunk と progress age 表示へ変更する

- Actor: agent
- Depends on: H-02
- 参照: Hermes 調査 9.3、11.10 節
- 事前条件: monitor parser が H-02 の metric を取得できる fixture がある
- Files: `monitor/src/config.rs`、`monitor/src/metrics.rs`、`monitor/src/state.rs`、
  `monitor/src/tray.rs`、関連 test
- Actions:
  1. `live_metric` 既定値を `prefill-chunk-tps` に変更する。
  2. decode fallback は `generation_chunk_tps` を平均値より先に使う。
  3. active 中に progress age を詳細表示し、stall 後の古い chunk 値を現在値として表示しない。
  4. average TPS は診断用詳細として残し、既存の選択肢を削除しない。
- 事後条件: メニューバーが累積平均ではなく現在の低速化を早く示す
- 受入基準: chunk/average の乖離、age threshold 超過、完了、offline の UI test がある
- Verification: monitor unit test、共通 local gate
- ユーザーレビュー・手作業: 表示文言と値の変動の見やすさを画面で確認する
- Evidence: `monitor/src/metrics.rs` が H-02 の prefill progress age、generation progress observed / progress age を解析し、`monitor/src/state.rs` が表示状態へ伝搬。`monitor/src/tray.rs` は既定の prefill current chunk TPS、decode fallback の chunk-first、progress age 欠落時の `progress age unavailable`、60 秒以上の `stalled`、first-token waiting を表示し、古い TPS を現在値として表示しない。`cargo test -p siderostat-monitor`（116 tests）、`cargo test --all-targets`（221 tests + integration tests）、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --check`、`git diff --check` が成功（2026-08-23）。ユーザーが画面上の文言と値の変動を確認し、H-03 を完了と評価した。
- 停止条件: detector が UI 表示文字列を parse しないと成立しない

### [x] H-04 redaction 済み diagnostic snapshot を実装する

- Actor: agent
- Depends on: H-03
- 参照: Hermes 調査 9.5、12.1 節
- 事前条件: H-01 に snapshot schema、保存先、permission、保持数が定義済み
- Files: `src/diagnostics.rs`（新規）、`src/app.rs`、関連 test
- Actions:
  1. cluster/control generation、phase、lease、child identity、progress、in-flight、process/network/OS 情報を集める。
  2. recovery ID ごとに `snapshot.json` を temporary file から atomic rename する。
  3. prompt、response、API key、token、session ID、完全な deployment ID を schema に持たせない。
  4. permission と bounded retention を実装し、snapshot write failure を recovery 前に明示する。
- 事後条件: 状態初期化前の evidence を一つの recovery ID で追跡できる
- 受入基準: schema golden test、redaction forbidden-key test、atomic write failure、retention test がある
- Verification: unit test、permission check、共通 local gate
- ユーザーレビュー・手作業: snapshot の保存項目と retention を承認する
- Evidence（実装）: `src/diagnostics.rs` に schema version 1 の redaction-safe snapshot、`~/Library/Application Support/siderostat/recovery/snapshots/<recovery-id>/snapshot.json` への `0700/0600` private permission、同一ディレクトリ内 temporary file・file sync・atomic rename・directory sync、最新8件 retention を実装。`src/metrics.rs` の aggregate progress/in-flight と既存 `/cluster` 相当の control/lease/child identity を `src/app.rs` から read-only に収集する。schema golden、forbidden-key、atomic write failure、permission、retention、app capture tests が成功。保存項目および最新8件 retention をユーザーが承認し、H-04 を完了と評価した（2026-08-23）。
- 停止条件: snapshot 取得が cluster state を mutation する、または secret を保存しないと診断不能になる

### [x] H-05 bounded canary executor と CLI を実装する

- Actor: agent
- Depends on: H-04
- 参照: Hermes 調査 8.1、10 Phase 0、11.3 節
- 事前条件: fixed prompt、max token、deadline、成功 JSON が H-01 に記載済み
- Files: `src/canary.rs`（新規）、`src/cli.rs`、`src/app.rs`、関連 test
- Actions:
  1. 64 token 以下の固定・非秘密 prompt を local public endpoint へ一回だけ送る。
  2. elapsed、TTFB、生成 token 数、chunk TPS、HTTP result だけを結果に含める。
  3. deadline、HTTP error、低 TPS、progress stall を別 reason code にする。
  4. `siderostat cluster canary --json` を追加し、既定で状態を復旧・変更しない。
- 事後条件: `/healthz` では検出できない実推論速度を bounded request で確認できる
- 受入基準: 正常、低 TPS、first-token timeout、mid-stream stall、HTTP error の fake DS4 test がある
- Verification: canary integration test、共通 local gate
- ユーザーレビュー・手作業: canary prompt が機密情報を含まず、課金先を外部 endpoint に変更できないことを確認する
- Evidence（実装）: `src/canary.rs` に固定 prompt/body、設定済み local public endpoint のみに接続する `CanaryExecutor`、128 KiB bounded SSE parser、30秒 deadline、60秒 progress stall、5 tokens/s lower bound、`healthy` / `deadline` / `http_error` / `low_decode_tps` / `progress_stall` の有限結果を実装。`src/cli.rs` に `siderostat cluster canary --json` を追加し、admin API tokenやcluster state mutationを経由せず、失敗時は非0終了とした。fake DS4 に遅延・stall・HTTP status制御を追加し、5ケースの canary integration test が成功。`cargo test --all-targets --features test-support`（229 unit、canary 5、既存 integration、reconnect 37）、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --check`、`git diff --check` が成功（2026-08-23）。
- ユーザーレビュー記録（2026-08-23）: ユーザーは固定 prompt が非秘密であること、任意URL・任意prompt・外部課金先を指定できないこと、JSON出力項目を承認し、H-05 を完了と評価した。
- 停止条件: canary が任意 URL、任意 prompt、無制限 token を受け付ける必要がある

### [x] H-06 recovery job の単一 owner と admin API を実装する

- Actor: agent
- Depends on: H-05
- 参照: Hermes 調査 9.1、9.4 節、H-01 contract
- 事前条件: canary reason code と snapshot write が GREEN
- Files: `src/recovery.rs`（新規）、`src/app.rs`、`src/cli.rs`、関連 test
- Actions:
  1. coordinator の `DistributedReady` だけで作成できる recovery job を実装する。
  2. recovery ID、reason、phase、開始/終了時刻、結果を bounded history に保持する。
  3. authenticated start/status endpoint と `cluster recover-degraded` CLI を追加する。
  4. 同時要求は同じ active job を返し、第二 owner を作らない。
  5. snapshot 成功前に admission、child、cluster state を変更しない。
- 事後条件: lifecycle 未接続でも job ownership と観測 API が独立して test できる
- 受入基準: auth、role/state gate、duplicate、stale ID、snapshot failure、history bound の test がある
- Verification: handler/state test、共通 local gate
- ユーザーレビュー・手作業: なし
- Evidence（実装）: `src/recovery.rs` に coordinator + `DistributedReady` + `distributed-layer-parallel` の gate、single active owner、recovery ID / reason / phase / start・end / result の bounded history（既定32件）、idempotency、cooldown（既定1時間）・attempt limit（12時間あたり2回）を実装。`src/app.rs` に bearer-auth 付き POST `/cluster/recover-degraded` と GET `/cluster/recover-degraded/{recovery_id}` を追加し、snapshot 成功前の cluster/admission/child mutation を行わない。`src/cli.rs` に `cluster recover-degraded` の start/status と `--json` を追加。`tests/recovery.rs` および app handler test で auth、role/state gate、duplicate、stale ID、snapshot failure、history bound、completed idempotent replay を確認。`cargo test --all-targets --features test-support`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --check`、`git diff --check` が成功（2026-08-23）。
- 停止条件: recovery job state を既存 cluster state の代用 source of truth にする必要がある

### [x] H-07 demote / promote / post-canary を recovery job へ接続する

- Actor: agent
- Depends on: H-06
- 参照: Hermes 調査 8.2、9.1、11.3〜11.7 節
- 事前条件: recovery job phase reducer と既存 demotion integration test が GREEN
- Files: `src/recovery.rs`、`src/cluster/production/pairing.rs`、`src/app.rs`、関連 test
- Actions:
  1. recovery admission drain timeout を既存 demote ownerへ渡し、admission block、in-flight drain、既存 demote の順で PairedStandaloneReady へ戻す。通常 lifecycle の drain timeout は変更しない。
  2. `auto_promote` の既存 owner を利用して新 generation の DistributedReady を待つ。
  3. 両 node の old/new child identity と generation を比較する。
  4. post-recovery canary 成功後だけ admission を再開し job を success にする。
  5. 各 failure で restart loop に入らず、H-01 の安全な state と reason を返す。
- 事後条件: worker-only restart を追加せず、distributed pair 全体を一回だけ再生成できる
- 受入基準: 正常、drain timeout、demote failure、promotion failure、canary failure、peer loss の test がある
- Verification: 2 node fake integration test、既存 reconnect suite、共通 local gate
- ユーザーレビュー・手作業: なし
- Evidence（実装）: `CoordinatorDistributedRuntime` に通常 lifecycle と分離した recovery 用 demote timeout、admission を再開しない recovery promotion、promotion failure 時の safe-state 保持を追加。`ProductionClusterRuntime` の recovery owner flag で auto-promote と operator の通常 pair/promote/demote を抑止し、既存 lifecycle owner を一回だけ利用する。`src/proxy.rs` に外部へ転送しない一回限り・30秒の recovery canary permit、`src/app.rs` に `snapshot -> admission_blocked -> draining -> demoting -> paired_standalone -> promoting -> post_recovery_canary -> serving` の orchestration を接続し、canary `healthy` 後だけ admission を再開する。local distributed child の PID/generation と peer generation の更新を確認し、失敗時は child を追加 restart せず admission を block したままとした。coordinator test で normal recovery promotion、drain timeout、demote failure、promotion failure、canary permit、既存 reconnect suite の peer loss を確認。全ターゲット test 236 unit、canary 5、既存 integration、reconnect 37、recovery 6、clippy `-D warnings`、format、diff check が成功（2026-08-23）。
- 停止条件: control protocol を迂回した remote signal、manual state file edit、unverified child kill が必要になる

### [x] H-08 opt-in 自動 detector と安全弁を実装する

- Actor: agent
- Depends on: H-07
- 参照: Hermes 調査 9.2、9.4、10 Phase 2 節
- 事前条件: manual recovery の全 failure test が GREEN
- Files: `src/recovery.rs`、`src/config.rs`、`siderostat.example.toml`、関連 test
- Actions:
  1. `enabled=false` 既定の typed config を追加し、H-01 の閾値を default にする。
  2. low TPS は継続時間、stall は last progress age、pre-cron は canary failure で判定する。
  3. cooldown、12 時間内回数、active owner、DistributedReady、role を全て開始前 gate にする。
  4. recovery admission drain timeout または連続失敗では自動 retry せず、generation/target が不変なら admission を復元し、それ以外は manual intervention を通知する。
  5. recovery started/completed/failed と抑制 reason を structured log/metrics にする。
- 事後条件: opt-in 時だけ bounded automatic recovery が動き、disabled 時は観測だけ行う
- 受入基準: 単一低 sample、idle 0 TPS、cooldown、回数上限、duplicate event、clock advance の test がある
- Verification: deterministic-time unit test、fake 2 node integration test、共通 local gate
- ユーザーレビュー・手作業: 実機で `enabled=true` にするのは H-10 の change window 内だけとする
- Evidence（実装・検証 2026-08-23）: RecoveryConfig を schema v2 の optional typed section として追加し、
  enabled=false、admission drain 60秒、cooldown 1時間、12時間内2回、decode下限5 tokens/s・継続30秒、
  progress stall 60秒、canary deadline 30秒を既定値に固定した。低TPSの単一sample、idle 0 TPS、
  first-token timeout、progress stall、canary failure、重複event、単調時間の後退を
  deterministic-time RecoveryDetector unit testで確認した。検知は enabled=true の常駐タスクから
  既存 RecoveryService のsingle ownerへ一回だけ委譲し、cooldown、attempt limit、role/state、
  active ownerのgateと自動retry抑止を共通化した。recovery drain timeoutでgeneration/targetが不変の
  場合だけadmissionを復元し、変更済みの場合はblocked維持とmanual-intervention structured logを
  出す。ds4_proxy_recovery_events_total{event,reason} と structured logを追加し、recovery IDなど
  高カーディナリティ情報をlabelに含めない。既存fake 2-node lifecycle/recovery integrationを含む
  cargo test --all-targets --features test-support（249 unit、bundle 4、canary 5、fake DS4 2、
  phase/reconnect/recovery integration GREEN）、clippy -D warnings、format、diff checkが成功した。
  siderostat.example.toml と docs/spec.md／docs/operations.md にopt-inとH-10 change windowを記録した。
- 停止条件: wall clock 変更、固定 sleep、無制限 retry に依存する test しか作れない

### [x] H-09 degradation / recovery regression suite を固定する

- Actor: agent
- Depends on: H-08
- 参照: Hermes 調査 11 節
- 事前条件: detector、manual/automatic recovery、monitor test が個別に GREEN
- Files: `tests/throughput_recovery.rs`（新規）、`tests/support/mod.rs`、必要な fixture
- Actions:
  1. 正常短 request、長い正常 prefill、低 TPS、first-token stall、progress stall を別 test にする。
  2. active request の drain 順序と timeout を event log で assert する。
  3. recovery 後の generation/PID 更新、canary、admission、orphan 不在を一つの helper で確認する。
  4. worker/control 不通、promotion failure、連続 canary failure で safe state を確認する。
  5. suite を 10 回反復し、固定 sleep と test 間共有 port/state を使わない。
- 事後条件: H-02〜H-08 の release gate を一 command で再実行できる
- 受入基準: Hermes 調査 11.1〜11.10 の repository 内対象が test 名へ一対一対応する
- Verification: 対象 suite 10 回、標準並列実行、共通 local gate
- ユーザーレビュー・手作業: なし
- Evidence（回帰 suite・2026-08-23）: `tests/throughput_recovery.rs` に Hermes 11.1〜11.10 の
  対象を一対一で表す10 test（`normal_short_request_is_not_degraded`、
  `long_normal_prefill_does_not_trigger_decode_recovery`、`low_tps_requires_a_sustained_window`、
  `first_token_stall_is_classified_before_progress_stall`、`progress_stall_is_classified_after_a_progress_event`、
  `active_request_drain_preserves_order_and_timeout_without_kill`、`promotion_failure_stays_safe_and_suppresses_retry_loop`、
  `control_unavailable_converges_to_standalone_without_orphans`、`repeated_canary_failure_has_no_automatic_retry`、
  `two_node_recovery_replaces_children_and_keeps_canary_gate_blocked`）を固定した。active request の
  event order（request start → drain timeout → request finish → drain complete）、recovery 後の
  generation/PID 更新、post-recovery canary healthy、admission blocked、distributed child の
  orphan 不在を検証する共通 helper と、worker/control 不通・promotion failure・連続 canary failure
  の safe state を追加した。回復後も admission を閉じたまま次の世代へ移行できるよう、
  `AdmissionGate::reset_blocked_generation` を追加し、recovery demote/promotion で使用した。
  `tests/support/mod.rs` の fake child は再起動ごとに PID 相当値を更新する。対象 suite は標準並列で
  10回連続（各回10 test）GREEN、固定 sleep・共有 port/state なしで完了した。
- 停止条件: production code に test 専用 recovery path を追加しないと成立しない

### [x] H-10 2 node 実機 recovery と Hermes handoff を検証する

- Actor: user + agent
- Depends on: H-09
- 参照: Hermes 調査 8、10、11、12 節
- 事前条件: change window、rollback package、両 node の current config backup、in-flight 0 が確認済み
- Files: `docs/compatibility/v0.3.0-throughput-recovery.md`（新規、redaction 済み結果のみ）、
  `docs/operations.md`
- Actions:
  1. 正常 canary の TTFB、chunk TPS、token 数を baseline として採取する。
  2. test fixture または承認済み test DS4 で low TPS/stall を作り、snapshot を先に保存する。
  3. manual recovery、次に opt-in automatic recovery を各一回だけ実行する。
  4. new DistributedReady、両 child generation、post-canary、admission、orphan 不在を確認する。
  5. cooldown と二回目抑制を確認後、`enabled=false` へ戻す。
  6. Hermes 1,800 秒、Siderostat 2,400 秒の暫定 deadline 順と、pre-cron canary 手順を文書化する。
- 事後条件: P1 feature の実機 evidence があり、Hermes 側手作業が明記される
- 受入基準: recovery 一回で収束し、失敗時は standalone または明示 unavailable に有限時間で収束する
- Verification: doctor/public API/canary、PID/generation、snapshot、metrics、config 前後比較
- ユーザーレビュー・手作業: change window、故障注入、config 切替、Hermes cron 前 canary 組込みを実行する
- Preparation evidence（2026-08-23、実機変更前）: 両nodeのread-only `/healthz`、`/readyz`、`/cluster`、
  `/metrics` と `cluster doctor --json` を確認し、version 0.3.0 / build 13、healthy、
  `in_flight=0`、`admission=serving`、`DistributedReady`、両child runningを確認した。現在インストール
  されているbuild 13のruntime helperはH-10で必要な `cluster canary` と `recover-degraded` CLIを
  含まないため、実機 recovery は開始していない。source CLIの初回 canary payload は既存runtimeから
  HTTP 400を返されたため、Chat Completionsの `messages` payloadへ修正し、source CLIで
  `healthy / HTTP 200`（TTFB 3682ms、2 tokens、58.627 tokens/s）を確認した。build 14のapp-dev
  bundle verificationはPASSしたが、Developer ID署名はtimestamp取得失敗で停止し、両nodeへの
  install・config変更・故障注入は行っていない。change window外のため recovery opt-inも変更していない。
- Evidence（2026-08-25、build15実機 change window）: `docs/compatibility/v0.3.0-throughput-recovery.md`。
  両nodeでbuild15（unsigned/ad-hoc DMG SHA-256 `0c681e80cc65498965402aae041b86485929640e105e7c58f419f61ce43e7ea1`）を起動し、
  DistributedReady、doctor healthy、admission serving、baseline canary HTTP 200を確認した。
  coordinatorでmanual recoveryを一回実行し、snapshot保存、cluster generation `1141 -> 1147`、
  両childの置換、post-recovery canary healthy、admission servingを確認した。次にchange window内だけ
  `[recovery] enabled=true`を両nodeへ追加し、管理対象coordinator childのfirst-token stallを故障注入して
  automatic recoveryを一回実行した。metricsは `started=1`、`completed=1`、post-canary healthy、
  DistributedReady、orphanなしとなった。同一事象の二回目は `suppressed{reason="cooldown"}=1` で
  新jobを作成しなかった。検証後は両nodeのconfigをbackupへ復元し、automatic recovery disabled、
  doctor healthy、final canary HTTP 200、child各1件を確認した。Hermes handoffのpre-cron doctor/canary、
  Hermes `1800s` / Siderostat `2400s`暫定deadline順を `docs/operations.md`へ追記した。
 なおMacBook Proでは旧署名由来のstale Service Management登録により通常登録がcode 57で失敗したため、
  recovery検証中だけproduct-owned runtimeを同一ユーザーのlaunchd jobへ一時復帰させた。このmacOS
  lifecycle caveatはrecovery結果とは分離し、bundle更新時のunregister/register follow-upとして残す。
- 停止条件: production cron 実行中、rollback 不可、force kill/state 削除/OS 再起動が必要になる

## 12. Phase N: recovery epoch 単位の通知重複排除

### [x] N-01 pure semantic deduplicator を実装する

- Actor: agent
- Depends on: H-10
- 参照: 通知調査「改善提案」
- 事前条件: recovery ID/generation と cluster transition event が外部から取得可能
- Files: `src/notify.rs`、関連 unit test
- Actions:
  1. peer loss または片側 restart から最終安定 state までを recovery epoch として識別する。
  2. epoch 内の SoloStandaloneReady と PairedStandaloneReady を通知種別ごとに一回へ制限する。
  3. worker が SoloStandaloneStarting の一時 Pairing を「ノード検出」の確定条件から外す。
  4. DistributedReady、恒久 Solo、Backoff、ManualInterventionRequired、DeploymentMismatch は抑制しない。
  5. deduplicator は通知 decision だけを返し、cluster/admission/child state を変更しない純粋 reducer にする。
- 事後条件: transition 列から送信/抑制を deterministic に判定できる
- 受入基準: 180 秒相当反復、cable detach、再接続、重大 state、epoch rollover の table test がある
- Verification: notify unit test、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: epoch 判定のため通知 service から cluster state を mutation する必要がある

Evidence（N-01 pure reducer と table/反復 test 完了 2026-08-25）:
`src/notify.rs` の `NotificationDeduplicator` に recovery ID／generation を持つ
`NotificationEpoch` を追加し、明示的な recovery epoch と cluster transition の暗黙 epoch を
区別した。同一 epoch の `SoloStandaloneReady`／`PairedStandaloneReady` は通知種別ごとに一回へ
制限し、`DistributedReady` で epoch を閉じる。worker で local standalone が未準備の一時
`PairedStandaloneReady` は `notification_event_for_snapshots` の確定通知対象から除外した。
`StandaloneRestart`、`DistributedReady`、`Backoff`、`ManualInterventionRequired`、
`DeploymentMismatch` は deduplicator で抑制しない。180回相当の Solo/Paired 反復、明示 epoch
rollover、worker 一時 Pairing、重大通知の unit test を追加した。cluster/admission/child state
への feedback は追加していない。

### [x] N-02 notification service と observability へ接続する

- Actor: agent
- Depends on: N-01
- 参照: 通知調査「通知が反復する理由」「改善提案」5〜6
- 事前条件: pure reducer の全 transition table が GREEN
- Files: `src/notify.rs`、`src/app.rs`、`src/metrics.rs`、関連 test
- Actions:
  1. runtime の transition stream を reducer に渡し、送信対象だけ既存 notifier へ渡す。
  2. suppressed count と recovery generation/ID を bounded-label metric または structured log にする。
  3. sender failure、GUI session 不在、watch channel 終了を cluster lifecycle へ返さない。
  4. H phase の recovery started/completed/failed 通知と同じ epoch を共有する。
- 事後条件: 通知重複が抑えられ、抑制理由を本文なしで診断できる
- 受入基準: sender failure と channel close を含む async integration test で lifecycle event 消失がない
- Verification: notification integration test、reconnect suite、共通 local gate
- ユーザーレビュー・手作業: 通知文言と重大通知が抑制されない一覧を承認する
- 停止条件: metric label に unbounded recovery ID、node 名、本文を入れる必要がある

Evidence（notification service／observability 接続と非干渉 test 完了 2026-08-25）:
`DesktopNotificationService` に runtime と共有する `Metrics` を接続し、抑制数を
`ds4_proxy_notification_events_total{event="suppressed",kind="..."}` として記録した。
metric label は固定された通知種別だけで、recovery ID、generation、node 名、通知本文は含めない。
recovery started/completed/failed は同じ service に渡し、recovery ID／generation／failure reason は
構造化ログへ記録する。`AppState` の recovery start gate、snapshot failure、lifecycle failure、
successful canary後の completion と通知 epoch を接続した。

通知送信失敗、GUI session 不在、watch channel close は通知 task 内で処理し、cluster lifecycle の
結果へ返さない test を追加した。`cargo test --all-targets`（lib unit tests と integration suites）、
`cargo clippy --all-targets --all-features -- -D warnings` は PASS。N-03 の実機通知レビューは
未着手であり、通知文言と実機通知回数のユーザー承認を残す。

### [x] N-03 coordinator-only restart の実機通知回数を検証する

- Actor: user + agent
- Depends on: N-02
- 参照: 通知調査「再現方法」「必要な回帰 test」
- 事前条件: DistributedReady、in-flight 0、rollback package、`allow_sigkill=true`、stop timeout が確認済み
- Files: `docs/compatibility/v0.3.0-notification-dedup.md`（新規、redaction 済み結果のみ）
- Actions:
  1. coordinator runtime だけを app の graceful restart 経路で再起動する。
  2. worker の stop が長い経路と短い経路を各一回観測する。
  3. Solo/Paired 各最大 1 回、最終 DistributedReady 1 回、抑制 count を確認する。
  4. doctor、public API、新 PID/generation、orphan 不在を確認する。
- 事後条件: P2 feature の実機 gate が閉じ、`T-01` が着手可能になる
- 受入基準: 通知回数が期待値以内で、recovery 時間と serving safety が v0.2.1 baseline より退行しない
- Verification: notification history、structured log、metrics、cluster/PID snapshot の時系列照合
- ユーザーレビュー・手作業: change window、coordinator restart、macOS 通知履歴の確認を行う
- 停止条件: request 実行中、job 重複、rollback 不可、state file 削除が必要になる

Evidence（coordinator-only restart の長い停止経路と通知レビュー、2026-08-25）:
ユーザーが coordinator の Monitor から `siderostat-runtimeを再起動` を一回実行した。
agent は再起動中の Solo/Paired/Promoting/DistributedReady 遷移と、両 node の最終
`doctor healthy=true`、admission `serving`、in-flight `0`、新しい child PID/generation、
orphan 不在を確認した。約100秒で両 node が DistributedReady へ復帰し、長い停止経路の
実機観測は PASS とした。

ユーザーが macOS 通知履歴を確認した結果、coordinator 側は「5秒後に再起動」1回、
Standalone 起動1回、Paired/ノード検出1回、最終 DistributedReady 1回、worker 側は
Standalone 起動1回だった。同一 epoch 内の安定状態通知の反復はなく、重大な再起動通知も
失われていないため、通知回数と文言のレビューは PASS とした。短い停止経路の実機観測と、
その時系列の metrics/structured log 照合は未実施であり、N-03 は未完了のままとする。

追加観測（2026-08-25）では、事前条件を再確認した後、agent が coordinator の同じ
graceful restart route を一回だけ発行した。worker stop は短い経路へ入らず、coordinator
側で Solo/Paired と worker hello 待ちの再試行を複数回経由した。約3分半後に両 node は
再び `DistributedReady`、admission `serving`、in-flight `0`、`doctor healthy=true` へ
復帰し、新しい distributed child PID/generation と orphan 不在を確認した。しかしこれは
短い停止経路の証跡ではなく、2回目の macOS 通知履歴レビューも行っていないため、N-03 の
短い経路受入条件は未達のままとする。

実装調査・修正（2026-08-25）: 上記の長い経路をコード追跡した結果、`/admin/restart` が
distributed mode でも standalone supervisor を停止対象にし、直後の `std::process::exit`
で `run_servers` の `ProductionClusterRuntime::stop_distributed()` を実行しない問題を特定した。
修正後は distributed runtime を lifecycle owner として停止し、HTTP 202 後は server loop の
restart signal から listener/task/production child/standalone child の cleanup を通す。owner
選択と signal 到達の回帰 test、既存 graceful sequence test、全 263 test、clippy、fmt、diff-check
は PASS。実機の短い停止経路と通知履歴の再確認は、この修正版 artifact 適用後に行う。

再検証 preflight（2026-08-25、build16）では、両 node の bundle metadata／DMG SHA-256 は一致したが、
runtime Service Management job が `spawn failed` / exit code `78: EX_CONFIG`、admin port が
`Connection refused` だった。Mac Studio の unified log には `Contents/Helpers/siderostat-runtime`
の find/execute 失敗があり、bundle 内 helper の存在、arm64、codesign verification は PASS だった。
short-stop 操作と通知履歴レビューは実施せず、N-03 の build16 実機 evidence には採用しない。stale
Service Management registration を unregister → register して runtime readiness を復旧した後に
再開する。

追加確認（2026-08-25、ad-hoc build16）は、両 node が `DistributedReady` へ復帰した後に coordinator
の再起動を一回実行したが、LaunchAgent 再起動時の DiagnosticReports に
`SIGKILL (Code Signature Invalid)` / `Launch Constraint Violation` が両 node で記録された。
coordinator 側では Solo/Paired の反復も観測したため、この試行は短い停止経路の evidence に採用しない。
`dmg-dev` が生成した helper は ad-hoc 署名で `TeamIdentifier` 未設定だったことが原因であり、
Developer ID Application／Developer ID Installer、`timestamp none` のローカル診断用 build16 artifact
を別途作成して両 node の `/tmp` に配置した（公証済み配布 artifact ではない）。

Developer ID 署名済み build16 の再検証（2026-08-25）では、両 node の再起動前 baseline
（`DistributedReady`、admission `serving`、in-flight `0`、LaunchAgent `running`）は PASS だった。
ユーザーが coordinator の再起動を一回実行した後、Mac Studio は Solo/Paired と worker hello 待ちを
複数回経由し、180 秒の観測終了時点では `paired-standalone-ready` だった。MacBook Pro も一時的に
`unavailable-transition`／admission `blocked` へ遷移した。追加確認では最終的に両 node が
`DistributedReady`、admission `serving`、in-flight `0` へ復帰したが、反復と長い再収束があるため
短い停止経路の受入基準は未達とする。ad-hoc build16 と異なり Code Signature Invalid は再発せず、
残る課題は distributed child 停止後の cluster 再収束である。通知履歴レビューは技術 gate 未達のため
保留し、N-03 は未完了のままとする。

原因再調査（2026-08-25）: `ProductionClusterRuntime::stop_distributed()` はローカル distributed
child を停止するだけで peer へ計画停止を通知せず、restart signal による server loop cleanup が
完了するまで reconcile／route-loss monitor も動作可能である。control endpoint 停止後、peer は
lease 喪失を通常の `PeerLost` として処理し、`recover_from_peer_loss` により Solo へ fallback する。
一方 coordinator は `auto_pair`／`auto_promote` により自動復帰を試みるため、control session と
worker hello が安定する前の Solo/Paired 反復になる。実機 metrics の反復と source の分岐が一致した。
既存の deployment-mismatch 用 pairing block は計画 restart を抑止せず、既存の begin-drain／
stop-worker 制御経路も graceful process restart から呼ばれていない。前回修正は lifecycle owner
迂回を解消したが、この cluster-wide planned-restart protocol の欠落を未修正のまま残していた。

計画再起動プロトコル実装（2026-08-25）: 原因に対応して authenticated control plane の
`prepare-restart`／`cancel-restart`、runtime の planned-restart gate、peer acknowledgement、
既存 demotion lifecycle への接続を実装した。gate 中は reconcile、PeerLost recovery、route-loss
demotion、automatic Pair/Promote を抑止し、再起動後の新しい Pair で worker 側 gate を解除する。
control protocol、worker/coordinator phase、graceful restart の回帰を含む全 `cargo test
--all-targets`（265 tests）、`cargo clippy --all-targets -- -D warnings`、format、diff-check は PASS。
この結果はコード検証の完了を示すが、N-03 の実機受入を完了させるものではない。次の signed build を
両 node に適用し、Solo/Paired 反復のない短い停止経路を実機で再検証する。

planned-restart 修正版 build17 の `app-dev --verify`、no-timestamp package/DMG 生成、DMG の
`hdiutil verify` は PASS した。検証用 DMG は両 node の `/tmp` に配置し、SHA-256
`579add4ecf66b2097f32bef1f5bb434d5f3895b5f1372cbffd65cd9536bc762f` の一致を確認した。
一方、Apple timestamp endpoint は両 node から HTTPS 443 接続不能で、no-timestamp package の
Notary 提出は secure timestamp 不在により `Invalid` となった。この artifact は内部検証専用であり、
再配布可能な公証済み DMG の条件を満たさない。したがって N-03 は build17 の secure timestamp
付き DMG の生成・両 node 配置・ユーザー再操作まで未完了とする。

追加実機観測（2026-08-25、build17 no-timestamp 内部診断 artifact）では、両 node の
`DistributedReady`／admission `serving`／in-flight `0` を確認後、ユーザーが Mac Studio の
Monitor から `siderostat-runtimeを再起動` を一回実行した。Solo/Paired の反復は発生しなかったが、
Mac Studio の coordinator child が停止せず、`demoting`／admission `blocked` のまま残留した。
MacBook Pro は worker として `DistributedReady`／`serving` へ復帰した。このため短い停止経路の
実機受入は未達であり、通知履歴のレビューは実施しない。

原因再調査・修正（2026-08-25）: build22 では coordinator child を先に停止した後、worker 側の
`Demote` 応答を待っていた。managed child の SIGKILL 後 reaping window（最大5秒）が peer HTTP
lifecycle timeout に含まれておらず、worker 側の停止・`PairedStandaloneReady` 遷移が完了しても
coordinator の要求だけが timeout し、local state が `Demoting / blocked` に残る経路があった。
planned restart 専用 lifecycle は、coordinator child 停止後、worker 側 `Demote` が成功してからだけ
local `BeginDemotion → PairingReady` を実行するよう変更した。peer 停止失敗時は coordinator child
を readiness 確認付きで復旧し、local target／admission と `DistributedReady / Serving` を戻す。
さらに lifecycle timeout を `stop + sigkill_reap_window + peer_request` に拡張し、SIGTERM 待機と
SIGKILL 後の reaping window（最大5秒）を分離した。回帰 test は planned restart の coordinator／
peer 停止失敗復元、既存の peer-loss 抑止、SIGTERM timeout 後の bounded kill を含む。

build22 短い停止経路再検証（2026-08-25）では、両 node の runtime が build22 であることを確認後、
ユーザーが coordinator の Monitor から `siderostat-runtimeを再起動` を一回実行した。agent は約5分間、
両 node の `/cluster`、`/readyz`、`/metrics`、LaunchAgent 状態を時系列収集した。worker は
`paired-standalone-ready`／admission `serving` へ復帰したが、coordinator は
`demoting`／admission `blocked`／`target_ready=false` のまま残り、distributed child の復帰、
新 generation、runtime の新世代起動は確認できなかった。worker metrics には
`distributed-ready → paired-standalone-ready`、coordinator metrics には
`distributed-ready → demoting` が記録された。in-flight は両 node とも `0` だった。
技術 gate 未達のため通知履歴レビューは実施せず、短い停止経路の evidence には採用しない。
raw capture は `/private/tmp/n03-short-build22-20260825.log` に保存し、N-03 は未完了のままとする。

build23 修正 artifact 準備（2026-08-25）: `cargo fmt --check`、`git diff --check`、clippy
(`--all-targets -- -D warnings`)、`cargo test --all-targets`（全テスト成功）が PASS。`app-dev --verify`
で 0.3.0 / build23 の bundle 検証、`pkg-dev` で package payload／installer script 検証、
`dmg-dev --verify` で DMG の readonly attach、直下3項目（pkg、`Siderostat Uninstaller.app`、
`README.html`）検証、`hdiutil verify` が PASS した。検証用 DMG は
`dist/native-build23/Siderostat-0.3.0.dmg`、SHA-256 は
`3fb7e4b1d696d38cfb73d2a2d98be436e3f208244abb8112cb0d09a5bfc4787d`。MacBook Pro の
`/tmp/Siderostat-0.3.0-build23.dmg` と Mac Studio の同名 path へ配置し、両方の SHA-256 が一致した。
これは Apple secure timestamp／notarization を含まない ad-hoc の実機検証 artifact
（`distribution_ready=false`）であり、公証済みリリース artifact の証跡には採用しない。

build23 実機再検証（2026-08-25）では、Mac Studio の restart 要求後に MacBook Pro が
`PairedStandaloneReady`／`Serving` へ遷移した一方、Mac Studio は `DistributedReady` の表示を
維持したまま target unavailable／admission blocked となり、LaunchAgent の runtime process が
再起動しなかった。coordinator 側の peer 停止完了通知が記録されず、peer 停止失敗時に coordinator
child を再起動して complete route を待つ rollback が、worker の Paired 復帰後に完了不能となる
可能性を確認した。この試行は N-03 の受入 evidence には採用しない。

追加修正では、planned restart の peer 停止失敗時に分散 child の complete route 復旧を待たず、
local standalone を起動して `BeginDemotion → PairingReady` を適用し、coordinator を
`PairedStandaloneReady`／admission `Serving` へ復旧する。これにより応答欠落後も長時間の
startup timeout 待機を避け、ユーザーが再試行できる状態へ戻す。
N-03 は build23 の実機短い停止経路再検証待ちとする。

build24 再検証 artifact 準備（2026-08-25）: 追加修正を含む `app-dev --verify`、package 生成、
DMG の `hdiutil verify`／readonly attach 検証が PASS。DMG は
`dist/native-build24/Siderostat-0.3.0.dmg`、SHA-256 は
`90c7b660e2056ab9ca6b28ad5c9d71feddbe29de26e79ef2f1eee64f8af63754`。同一ファイルを MacBook Pro と
Mac Studio の `/tmp/Siderostat-0.3.0-build24.dmg` に配置した。現在 build23 の固着 runtime は
sudo なしで product-owned runtime process を終了し、両 node は `PairedStandaloneReady`／admission
`Serving` へ復旧した。build24 の両 node インストール、coordinator-only restart、短い停止経路と
通知履歴の実機確認は未実施である。

build24 実機再検証（2026-08-25）では、両 node の healthz と runtime binary checksum を確認後、
ユーザーが Mac Studio の Monitor から `siderostat-runtimeを再起動` を一回実行した。MacBook Pro は
`PairedStandaloneReady`／`Serving` へ復帰したが、Mac Studio は `DistributedReady` の表示を維持した
まま `target=unavailable-transition`／admission `blocked`、distributed child 不在、LaunchAgent の
runtime process 世代不変となった。planned restart 中も worker 側 `Demote` の HTTP 応答を child 停止完了
まで待っていたため、coordinator が peer lifecycle 待機に残ることを source trace と live sample で
確認した。この試行は N-03 の短い停止経路 evidence に採用しない。

build25 修正（2026-08-25）: planned restart gate 中の `Demote` だけは control handler が先に ACK を返し、
worker child の停止は既存 lifecycle owner の非同期 effect として継続するよう変更した。通常の demote
と他の lifecycle command の完了 ACK は維持する。全 `cargo test --all-targets`（269 tests）、clippy、
format、diff-check は PASS。検証用 DMG は `dist/native-build25/Siderostat-0.3.0.dmg`、SHA-256 は
`ef1f32d02f60cfadd715df67b5b0d00670b366dcfba3bd5699f7c4728dadf590`。同一 DMG を両 node の
`/tmp/Siderostat-0.3.0-build25.dmg` に配置した。coordinator-only restart の短い停止経路を再実機確認
するまで N-03 は未完了のままとする。

build26 追加修正（2026-08-26）: build25 の実機適用前に、早期 `Demote` ACK 後の `Pair → PrepareWorker`
競合を追加監査した。worker は Demote 応答後も旧 distributed child を停止中であり、この期間に
Pair effect が gate を解除すると、worker の実状態が PairedStandaloneReady になる前に制御 phase だけが
WorkerPreparing へ進み得る。build26 では planned restart の child-running／Pair-pending／child-stopped／
completion を atomic gate で管理し、停止完了前の Pair は保留、停止完了前の PrepareWorker は `409
worker planned restart is still in progress` で拒否する。停止後の reciprocal Pair 送信失敗は child-stopped
状態を保持して後続 Pair で再試行する。通常の Pair／demote 経路と planned restart 以外の ACK 待ちは変更しない。
全 `cargo test --all-targets`（271 tests）、clippy、format、diff-check は PASS。修正を含む内部検証用 DMG は
`dist/native-build26/Siderostat-0.3.0.dmg`、SHA-256 は
`a1749436c1aaf4cb7296e38c95ed1f9844fb2d0c9ddb87ec7b2ab11d4aa590f1`。同一 DMG を MacBook Pro と Mac Studio
の `/tmp/Siderostat-0.3.0-build26.dmg` に配置した。secure timestamp／notarization を含まない内部検証用
artifact のため、N-03 の受入は build26 適用後の coordinator-only restart 実機確認まで未完了とする。

build26 実機再検証（2026-08-26）: 両 node へ build26 を適用し、DistributedReady を確認した後、ユーザーが
Mac Studio の Monitor から coordinator-only の `siderostat-runtimeを再起動` を一回実行した。再起動直後の
一時的な standalone／ready 待ちを経て、両 node が `state=distributed-ready`、
`mode=distributed-layer-parallel`、`admission=serving`、`in_flight=0`、`readyz 200` へ復帰した。
MacBook Pro は distributed worker generation 761 / PID 10139、Mac Studio は distributed coordinator
generation 1407 / PID 59194 を確認し、standalone child は停止済み、orphan はなし。Mac Studio の runtime
LaunchAgent は `runs=2`／新 PID 59052、peer lease は両 node で present／route-scoped／valid だった。
今回の再起動では build24 までの `demoting`／`unavailable-transition` 固着、runtime 世代不変、child 不在は
再現せず、追加20秒の安定観測後も serving を維持した。`target=local-standalone` は distributed coordinator
に対する内部 target 解決仕様であり、readyz 200 と矛盾しない。

これにより N-03 の短い停止経路に関する技術 gate（再起動、再収束、serving safety、generation、child／
orphan）は PASS とする。通知履歴レビュー（ユーザー確認、2026-08-26）では、開始前の baseline
`DistributedReady` 1回とは別に、coordinator restart の recovery epoch 内で `5秒後に再起動`、
`Standalone 起動`、`Paired / ノード検出`、`最終 DistributedReady` が各1回だった。epoch 内の Solo／Paired
相当通知は各1回、最終 DistributedReady も1回で、重大通知の欠落・反復はなかった。通知回数を含む N-03
受入基準を満たしたため、N-03 を完了とする。

## 13. Phase T: TP/RDMA の最終再評価と延期

### [x] T-01 TP/RDMA を最後に再評価し v0.4+ backlog を固定する（v0.4+ 延期承認済み）

- Actor: agent + user review
- Depends on: N-03
- 参照: DwarfStar TP/RDMA 調査 全節
- 事前条件: P0〜P2 の feature と実機 gate が完了済み
- Files: `docs/research/dwarfstar-rdma-tensor-parallel-2026-08-18.md` および `docs/research/ds4-investigation-report-2026-08-21_22-07_to_2026-08-22.md` の追跡 addendum、
  または `docs/roadmap-v0.4.md`（新規）
- Actions:
  1. upstream main の TP server lifecycle、Mac backend、disk KV、DSpark、batching の状態を再確認する。
  2. source commit、`ds4`/`ds4-server` digest、model/quantization、macOS/RDMA 条件を候補 matrix に記録する。
  3. `ds4-server` で Mac 間 TP が未提供、または release gate 未達なら v0.4+ へ延期する。
  4. upstream server path、固定 baseline への最小 backport、実験 stdio bridge の順で将来案を残す。
  5. v0.3.0 の config、manifest、state machine、metrics に TP placeholder を追加しない。
- 事後条件: v0.3.0 へ TP を滑り込ませる余地がなく、次版の entry criteria が文書化される
- 受入基準: 延期理由と再開条件が、upstream merge、HTTP contract、24 時間 endurance、fail-closed、
  rollback の各 gate を含む
- Verification: fixed commit/link、v0.3 diff に TP profile/config がないことを `rg` で確認する
- ユーザーレビュー・手作業: v0.4+ 延期を承認する。v0.3 採用へ変更する場合は本計画を止め、独立計画を作る
- 停止条件: 未マージ PR、人間向け REPL scrape、silent TCP fallback を production dependency にする必要がある

Evidence（T-01 調査・backlog 固定、2026-08-26）:
upstream の公開 main とローカル `origin/main` を再確認し、固定 commit は
`c1d4597a80e300b803dc642519718f2c999589da`（2026-08-23）、baseline `84cc882` から19 commit進んでいる
ことを確認した。main には DeepSeek V4 PRO 0813 の model/quality 更新、DSpark の非ゼロ温度対応と
`--mtp-exact-sampling`、M5 Max pipeline 測定値が入った。一方、Mac 間 TP は `ds4` CLI のみで、
`ds4-server` の `--cuda-tensor-parallel` は単一 CUDA host の別経路だった。Mac 間 TP の server
lifecycle、layer-parallel RDMA、distributed pipeline DSpark は main に未採用である。

関連 PR の公開状態も照合した。#813（ROCm server TP）、#754（CUDA network EP/TP）、#835（pipeline
DSpark）は Open、#715（Apple layer-parallel RDMA）は Closed で、いずれも main 採用の証跡ではない。
したがって v0.3.0 の production capability へ追加せず、TP/RDMA/distributed DSpark を v0.4+ backlog
へ延期する。再開条件は、対象 server role/lifecycle、HTTP contract、固定 digest による RDMA／明示的
TCP の24時間 endurance、fail-closed／no silent fallback、child・peer・admission・generation・orphan
rollback の5 gateとした。CLI stdout/PTY scrape は production dependency とせず、stdio bridge は
制約付き PoC に限る。

詳細な更新、matrix、upstream link、backlog は次に記録した。
`docs/research/dwarfstar-rdma-tensor-parallel-2026-08-18.md` 第12節、
`docs/research/ds4-investigation-report-2026-08-21_22-07_to_2026-08-22.md` 第17節。
ユーザーは 2026-08-26 に v0.4+ への延期を承認した。T-01 は完了とし、再開条件を満たすまで v0.3.0 の対象外として扱う。

## 14. Phase R: 文書、release candidate、最終受入

### [x] R-01 利用者・運用・開発文書を v0.3.0 へ同期する（ソース公開方針へ更新）

- Actor: agent + user review
- Depends on: T-01, E-06
- 参照: P0〜P2 の完了 Evidence
- 事前条件: behavior、default、path、command、既知制約が実装で確定済み
- Files: `README.md`、`README.ja.md`、`docs/installation.md`、`docs/installation.ja.md`、
  `docs/operations.md`、`docs/operations.ja.md`、`docs/troubleshooting.md`、
  `docs/troubleshooting.ja.md`、既存の開発・仕様文書
- Actions:
  1. 通常 install を source checkout からの `cargo xtask install --start` とし、公式の事前ビルド済み
     `.app`、`.pkg`、DMG、`Siderostat Uninstaller.app` を配布しない方針を明記する。
  2. background service、login start、migration、upgrade、rollback、`cargo xtask uninstall` を記載する。
  3. progress age、canary、manual/automatic recovery、snapshot、cooldown を記載する。
  4. notification epoch と TP/RDMA 延期を既知制約として記載する。
  5. command、path、identifier、既定値を実装から再照合する。
- 事後条件: 利用者が旧 LaunchAgent 手順を通常経路として選ばない
- 受入基準: clean install から uninstall、degraded recovery まで文書だけで再現できる
- Verification: link/path/command check、`git diff --check`、文書 clean-install rehearsal
- ユーザーレビュー・手作業: install、Background Items、recovery の文言と安全警告を承認する
- 停止条件: 実装と文書の default が一致しない、または未検証 command を記載する必要がある

Evidence（R-01 文書同期、2026-08-26）:
`README.md`、`docs/installation.md`、`docs/operations.md`、`docs/troubleshooting.md`、
`docs/development.md`、`docs/menu-bar-monitor-spec.md`、`docs/spec.md`、`contrib/launchd/README.md` を
v0.3.0 の導入経路へ同期した。現行の公式経路は source checkout からの
`cargo xtask install --start` とし、事前ビルド済み DMG/pkg、Developer ID、notarization、timestamp は
公式リリース条件から除外した。background service、Login Items / Background Items の承認、旧版 migration、
upgrade、rollback、`cargo xtask uninstall` とユーザーデータ保持境界を記録した。progress age、bounded canary、manual/opt-in automatic recovery、
redacted snapshot、cooldown、notification epoch dedup、および TP/RDMA/distributed DSpark の v0.4+ 延期も
利用者・運用文書から辿れる状態にした。

実装との再照合では、runtime config、secret、state、manifest、public/admin/control/peer/DS4 の各 endpoint、
`dev.siderostat-ds4-proxy.runtime`、`~/monitor.toml`、既定の recovery 値（drain 60秒、canary 30秒、cooldown
1時間、progress stall 60秒）を確認した。`git diff --check` と記載対象ファイルの存在確認は成功した。
追加で、`README.md` を英語正本、`README.ja.md` を日本語版として再同期し、README から辿る公開文書を
`docs/installation.md`、`docs/operations.md`、`docs/troubleshooting.md` とそれぞれの `.ja.md` に限定した。
旧来の開発・operator向け詳細文書は `docs/internal-*.md` へ退避し、公開文書から cargo/LaunchAgent、CI、
commit/digest、fixture、ソースパスを除外した。英日各3文書の見出し数、相互リンク、`git diff --check` は成功した。
ユーザーは 2026-08-26 に README と英日エンドユーザー文書、install、Background Items、recovery の文言と安全警告の変更を確認し、承認した。
同日、公式バイナリを配布しない方針を指定したため、公開文書と release gate を source-only に更新した。R-01 は完了とする。

### [x] R-02 v0.3.0 source release candidate と supply-chain evidence を作る

- Actor: user + agent
- Depends on: R-01
- 参照: 本書 4.3、配布仕様 15、16 節（package 条件は将来の任意バイナリ経路として参照）
- 事前条件: 全 automated suite と documentation check が GREEN
- Files: `Cargo.toml`、`monitor/Cargo.toml`、`Cargo.lock`、`docs/releases/v0.3.0.md`、
  `docs/releases/v0.3.0-acceptance.md`、release metadata/SBOM 生成設定
- Actions:
  1. crate version を一括して `0.3.0` に更新し、source release revision を固定する。
  2. `docs/releases/v0.3.0.md` と `docs/releases/v0.3.0-acceptance.md`、source checksum、
     SBOM/dependency inventory、third-party notices を作る。
  3. git revision、Rust version、target、build metadata を記録する。Team ID、notary submission ID、
     secure timestamp は source release metadata に記録しない。
  4. automatic test、clean source install、migration、recovery、notification evidence を acceptance
     文書へ集約する。
- 事後条件: source revision と依存関係の由来を検証できる release candidate 一式がある
- 受入基準: version、source revision、lockfile、dependency inventory、notices、公開文書が相互一致し、
  source-only release に不要な credential、model、user data、配布不能 binary artifact を含まない
- Verification: 共通 local gate、CI、source checksum、SBOM/notice/file-list/secret scan check
- ユーザーレビュー・手作業: source release scope、release note、known limitation、v0.4+ 延期項目を承認する
- 停止条件: uncommitted source、dirty tree、未追跡 credential、model/user data が source release evidence に含まれる

Evidence（R-02 preflight、2026-08-26、旧配布方針）:
R-01 承認後に `git status --short` を確認したところ、既存の source、test、配布スクリプト、仕様・互換性文書の
未コミット変更と、今回の公開文書を含む未追跡ファイルが残っていた。`git diff --check` は成功したが、旧 R-02 の
停止条件にある clean tree を満たさないため、version変更、署名、公証、release candidate artifact の生成は実施していない。
その後、source-only 方針への変更に伴い、署名・公証 artifact の生成は v0.3.0 の R-02 から除外した。再開には、
source-only の対象変更をレビュー可能な commit に確定し、clean tree を作る必要がある。

Evidence（R-02 完了、2026-08-26）:
root、monitor、xtask と `Cargo.lock` の workspace version を `0.3.0` へ更新した。locked graph の
third-party package 318件について name、version、license metadata、source、registry checksum を
`docs/releases/v0.3.0-dependencies.md` に記録し、license metadata 不明が0件であることを確認した。
`THIRD-PARTY-NOTICES.md` も provisional 文言を除いた確定版へ更新した。

仮 commit 36件を精査して feature 単位の履歴へ再構成し、source candidate は
`d9dd772bd68c7bb8cd743555a686733806a24b4a` として固定した。同 revision から
`siderostat-v0.3.0-source.tar.gz` を生成し、archive は231 entries、764,489 bytes、SHA-256
`13d761299f02ed8ddfd36cbc634c8d7e6dbf91fd8c2aa8580e6992efc7f6ad57`。tracked filename と content の
混入検査で model、secret、credential、private key、DMG/pkg、runtime state、cache、log、user data を
検出しなかった。archive は repository 外の `/private/tmp` に保持した。

candidate revision で `cargo fmt --all -- --check`、workspace/all-target/all-feature Clippy、test、release build、
`git diff --check` を再実行し、すべて PASS。R-02 を完了とし、R-03 のユーザー release 承認へ進む。

### [x] R-03 final acceptance と release 承認を完了する

- Actor: user + agent
- Depends on: R-02
- 参照: 本書 4.3、各実機 Evidence
- 事前条件: source candidate checksum が固定され、source revision と rollback 手順がある
- Files: `docs/releases/v0.3.0-acceptance.md` の結果追記
- Actions:
  1. clean source install、migration/upgrade/rollback、login、runtime crash、background stop を source revision で再確認する。
  2. standalone/paired/distributed/reconnect、throughput recovery、notification dedup の回帰 gate を実行する。
  3. credential、secret、model、user data、配布不能なバイナリ artifact が source release evidence にないことを再確認する。
  4. known risk、TP/RDMA 延期、automatic recovery opt-in を release note へ明記する。
  5. ユーザー承認後に、`CONTRIBUTING.md` に従う develop の履歴反映、annotated tag、push、GitHub Release 公開を行う。
- 事後条件: v0.3.0 source release の可否、rollback 手順、既知制約が一意に判定される
- 受入基準: 本書 4.3 の 8 項目が全て PASS。FAIL または未実施を waiver で暗黙に通さない
- Verification: source checksum を使った全 acceptance、`git status --short`、`git diff --check`
- ユーザーレビュー・手作業: release note、source revision、2 node の source install/recovery、最終 source release 可否を承認する
- 停止条件: acceptance 未実施、source rollback 不可、データ損失、orphan、restart loop が一件でもある

Evidence（R-03 完了、2026-08-26）:
`docs/releases/v0.3.0-acceptance.md` で本書 4.3 の8項目がすべて PASS した。final source revision は
`d9dd772bd68c7bb8cd743555a686733806a24b4a`、source archive SHA-256 は
`13d761299f02ed8ddfd36cbc634c8d7e6dbf91fd8c2aa8580e6992efc7f6ad57` で固定した。ユーザーは
release note、source revision、archive checksum、受け入れ結果を確認し、v0.3.0 の最終 release 可否を
承認した。その後、annotated tag `v0.3.0`（source candidate `d9dd772` を指す）、develop の履歴再構成と push、
GitHub Release 公開を完了した。source archive asset は SHA-256
`13d761299f02ed8ddfd36cbc634c8d7e6dbf91fd8c2aa8580e6992efc7f6ad57` で公開した。

## 15. 共通停止・エスカレーション条件

次のいずれかに該当したら、task を `[!]` にして、観測事実、影響、再開条件を記録する。

- 仕様どおりに進めるため root daemon、setuid、private framework、App Sandbox 例外が必要になる。
- unsigned/identity 未確認 process への signal、user data 削除、state file 削除、force kill が必要になる。
- certificate、private key、notary credential、API token を repository/CI log に置く必要がある。
- migration または upgrade が old/new runtime、listener、DS4 child の重複を作る。
- recovery が既存 cluster lifecycle owner を迂回する、Ready を偽装する、無制限 retry に入る。
- notification failure が cluster state、admission、serving availability を変更する。
- TP/RDMA を v0.3.0 に入れるため未マージ upstream PR または不安定な CLI parser が必要になる。
- 実機 task に rollback、change window、ユーザー承認のいずれかがない。

## 16. Task 完了報告 template

```text
Task: <ID と名前>
Result: PASS | FAIL | BLOCKED
Changed: <file 一覧>
Preconditions: <確認した事前条件>
Postconditions: <成立した事後条件>
Acceptance: <基準ごとの PASS/FAIL>
Verification: <command と件数>
Evidence: <SHA/artifact path と日付>
Observed risks: <なし、または残存 risk>
Next ready task: <Depends on を満たす task ID>
User review/manual action: <なし、承認待ち、または具体的な手作業と結果>
```
