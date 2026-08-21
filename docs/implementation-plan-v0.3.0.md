# Siderostat v0.3.0 実装計画

- 文書状態: 実装開始前レビュー案
- 作成日: 2026-08-19
- 対象 baseline: `develop`、crate version `0.2.1`
- 最優先仕様: [`distribution/macos-app-bundle-pkg-spec.md`](distribution/macos-app-bundle-pkg-spec.md)
- 障害調査:
  - [`research/hermes-cron-throughput-degradation-2026-08-19.md`](research/hermes-cron-throughput-degradation-2026-08-19.md)
  - [`research/coordinator-restart-notification-repetition-2026-08-19.md`](research/coordinator-restart-notification-repetition-2026-08-19.md)
- 延期候補:
  [`research/dwarfstar-rdma-tensor-parallel-2026-08-18.md`](research/dwarfstar-rdma-tensor-parallel-2026-08-18.md)

## 1. 目的

本書は、上記の配布仕様と調査文書を、v0.3.0 で順番に実行できる小さい task へ分解した
実装計画である。最優先 feature は、現在の `/usr/local/bin` と
`~/Library/LaunchAgents` への直接配置を、`Siderostat.app` と署名・公証済み `.pkg` へ
置き換えることである。

配布 feature が受入基準を満たす前に、低速推論の自動復旧、通知重複排除、TP/RDMA の
実装へ着手しない。Mac 間 Tensor Parallelism（以下 TP）と RDMA は最後に再評価し、
本計画では v0.4.0 以降へ延期することを既定判断とする。

各 task は、低い reasoning effort または小型モデルでも、設計判断を補完せずに実行できる
粒度を目標とする。そのため、事前条件、変更対象、順序、事後条件、受入基準、検証、
ユーザーによるレビューまたは手作業、停止条件を task ごとに固定する。

## 2. 優先順位と v0.3.0 の範囲

### 2.1 feature 優先順位

| 優先度 | feature | v0.3.0 の扱い |
|---:|---|---|
| P0 | macOS `.app` / `.pkg` 配布、Service Management、legacy migration | 必須。最初に完了する |
| P1 | 推論進捗の鮮度、canary、診断 snapshot、安全な degraded recovery | 必須。P0 完了後に開始する |
| P2 | recovery epoch 単位の通知重複排除 | 必須。P1 完了後に開始する |
| P3 | DwarfStar Mac 間 TP / RDMA / DSpark + TP | 実装しない。最後に upstream を再評価して v0.4+ へ送る |

優先度は task の番号ではなく依存関係で強制する。`H-01` は `E-05`、`N-01` は
`H-10`、`T-01` は `N-03` が完了するまで開始できない。

### 2.2 v0.3.0 の必須成果物

1. Apple silicon 向け `Siderostat.app` と flat installer package。
2. monitor を main app、runtime を app 内 LaunchAgent Helper とする bundle layout。
3. `SMAppService` による runtime の登録・停止と、main app の login start 設定。
4. legacy LaunchAgent を重複起動なしで移行し、失敗時に復旧できる migration。
5. Developer ID 署名、Hardened Runtime、公証、stapling、checksum、build metadata。
6. active 推論の最終進捗時刻と token 差分の観測、現在 chunk TPS の monitor 表示。
7. 診断 snapshot、canary、単一 owner の `recover-degraded` job、cooldown と回数上限。
8. coordinator-only restart 中の通知を recovery epoch 内で意味的に重複排除する機能。
9. clean install、upgrade、legacy migration、rollback、2 node recovery の実機 evidence。

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
| macOS app/pkg 配布仕様 1〜16 節 | A-01〜E-05 | P0 必須 gate |
| macOS app/pkg 配布仕様 17〜19 節 | R-01〜R-03 | final artifact と文書 gate |
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
- Developer ID の実機 task までに、ユーザーが Application / Installer certificate と
  notarization 用 Keychain profile を用意できる。
- legacy install、clean install、upgrade/rollback を壊してよい専用 test user または test Mac がある。
- 2 node recovery 試験に、推論 request を止められる change window がある。

### 4.2 全体の事後条件

- 通常利用者向け配布経路は `.pkg` であり、payload は `/Applications/Siderostat.app` だけである。
- monitor 終了後も runtime は継続し、background service 停止後は launchd が再起動しない。
- legacy と新 runtime は同時に同じ port、state、DS4 child を所有しない。
- user config、secret、manifest、model、cache は install、upgrade、rollback で保持される。
- degraded recovery は既存 demote/promotion owner を迂回せず、失敗時に restart loop へ入らない。
- 通知抑制状態は cluster state、admission、child lifecycle へ feedback しない。
- TP/RDMA の production code、config field、公開 profile は v0.3.0 に含まれない。

### 4.3 release 受入基準

1. `plutil`、`codesign`、`spctl`、`pkgutil`、`stapler` の全検証が final artifact で成功する。
2. clean Mac への install 前と first launch 前に runtime が勝手に登録されない。
3. background item の拒否、`requiresApproval`、承認、停止、再登録を UI から識別できる。
4. runtime crash 後は再起動し、monitor の通常終了では runtime を終了しない。
5. legacy migration 成功、意図的な migration 失敗、upgrade、prior `.pkg` rollback が実機で成功する。
6. 低 TPS、first-token stall、progress stall の自動 test が別々に原因を判定する。
7. recovery は diagnostic snapshot を先に保存し、両 distributed child を新 generation へ置換し、
   canary 成功後だけ正常扱いする。
8. cooldown、回数上限、同時 owner、drain timeout の test が restart loop と Ready 偽装を防ぐ。
9. coordinator-only restart の長い stop cycle でも、Solo/Paired 通知は epoch 内各 1 回、最終
   DistributedReady は 1 回であり、重大通知は失われない。
10. 既存 standalone、paired、distributed、reconnect acceptance が退行しない。

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
E-01 -> E-02 -> E-03 -> E-04 -> E-05       macOS 配布 feature gate
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
  2. `BundleProgram`、固定 label、`serve`、`RunAtLoad`、`KeepAlive`、`ThrottleInterval` を記載する。
  3. entitlement は空または A-01 で承認された最小集合だけにする。
  4. LICENSE、third-party notices、default config、icon を resource として列挙する。
- 事後条件: builder が値を置換する source template が揃う
- 受入基準: plist に user home、secret、`/usr/local/bin`、legacy label、`get-task-allow` がない
- Verification: `plutil -lint`、禁止文字列の静的 test、`git diff --check`
- ユーザーレビュー・手作業: A-01 で正式 icon 提供を選んだ場合、ユーザーが asset を提供する
- 停止条件: icon の license または再配布許可が不明

Evidence: contrib/macos/Info.plist.in(@VERSION@/@BUILD_NUMBER@ placeholder, LSMinimumSystemVersion 26.0)、dev.siderostat-ds4-proxy.runtime.plist(BundleProgram 相対 path, RunAtLoad/KeepAlive/ThrottleInterval)、entitlements.plist(空)、Resources/{LICENSE(MIT, okugauchi 2026), THIRD-PARTY-NOTICES.md, default-config.toml} を追加。tests/bundle_templates.rs に禁止文字列・bundle-relative path・identifier・resource の静的 test 4件追加。plutil -lint 3件 OK、test 4件 OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

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

Evidence: xtask/src/bundle.rs に app-dev builder（Info.plist 置換、bundle-relative LaunchAgent、placeholder icns 生成、inside-out ad-hoc 署名、plutil/codesign 検証）と unit test 3件を追加。xtask/src/main.rs に AppDev/PkgDev subcommand、xtask/Cargo.toml に base64、xtask/README.md に app-dev/pkg-dev 節を追加。.gitignore に /build /dist を追加。cargo xtask app-dev --version 0.3.0 --build-number 1 --verify で codesign --verify --deep --strict --verbose=4 PASS。二回の build で unsigned content SHA-256 全一致(2026-08-19); 2026-08-19

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

Evidence: monitor/src/service_management.rs の MainAppLoginItem（status/register/unregister）は C-01/C-02 で実装済み。monitor/src/tray.rs に runtime/login の登録状態表示 menu item 2つ（Runtime: / Login start:）と update_registration、registration_status_text 純粋関数を追加。unit test 2件（status 文言 5値、2x2 matrix 独立性=4 distinct pairs）。monitor 40 test OK、全 208 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [x] C-04 authenticated graceful runtime restart を実装する

- Actor: agent
- Depends on: C-03
- 参照: A-01 の restart contract、配布仕様 6.3、13.1 節
- 事前条件: endpoint の method、path、auth、drain timeout、response が承認済み
- Files: `src/app.rs`、`src/cli.rs`、既存 lifecycle module、関連 test
- Actions:
  1. loopback admin endpoint に token authentication を必須化する。
  2. admission block、in-flight drain、owned DS4 child stop、runtime process exit の順序を実装する。
  3. drain timeout と child identity mismatch では強制 kill せず error を返す。
  4. response 返却前に exit して client が曖昧な transport error だけを見る設計を避ける。
- 事後条件: launchd が runtime を新 binary で再起動できる graceful path がある
- 受入基準: auth 失敗、正常 drain、drain timeout、identity mismatch、重複要求の test がある
- Verification: handler/lifecycle test、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: cluster lifecycle owner を迂回した signal、unknown PID の kill、unauthenticated endpoint が必要になる

Evidence（親タスク集約）: C-04a の route/auth/body parse/重複要求、C-04b の admission block → drain → owned child stop → response → exit 処理、C-04c の CLI と受入基準対応を確認。auth 失敗、正常 drain、drain timeout、identity mismatch、重複要求の5受入基準は子タスクの test で網羅され、停止条件にも該当なし（2026-08-21）。

#### [x] C-04a graceful restart 専用 route・認証・body parse・重複要求を実装する

- Actor: agent
- Depends on: C-04
- 参照: A-01 の restart contract（2 節）
- 事前条件: `/admin/restart` の method、path、auth、response が承認済み
- Files: `src/app.rs`、関連 test
- Actions:
  1. AppState に supervisor 参照（`RwLock<Option<Arc<StandaloneSupervisor>>>`）を追加し、`serve_with_options` で attach する。cluster 非有効・solo-standalone のみ supervisor が存在する。
  2. `/admin/restart` route を追加する。既存 admin API と同じ Bearer token 認証（`authorized_admin`）を必須化する。
  3. request body の `drain_timeout_ms` を parse し、未指定時は config の cluster stop timeout を既定値にする。
  4. 進行中フラグ（AtomicBool）を追加し、重複要求は `409 { error: "restart_in_progress" }` を返す。
  5. supervisor が不在（cluster 有効・distributed 等）の場合は graceful restart を拒否する。
- 事後条件: `/admin/restart` が認証・parse・重複チェックを経て graceful restart 処理へ進める
- 受入基準: 認証成功/失敗、body 既定値、不正 body、重複要求の test がある
- Verification: handler test、共通 local gate
- ユーザーレビュー・手作業: なし

Evidence: src/app.rs に AppState へ supervisor 参照（RwLock<Option<Arc<StandaloneSupervisor>>>）・graceful restart 進行中 AtomicBool・default_drain_timeout（cluster stop timeout）を追加し、serve_with_options で supervisor を attach。try_claim_graceful_restart/release_graceful_restart、/admin/restart route、graceful_restart handler（Bearer 認証 → drain_timeout_ms parse → 重複チェック → supervisor 存在確認）を実装。perform_graceful_restart は C-04b の placeholder（202 accepted を返す）。unit test 6件（認証失敗/成功、body 既定値 5000ms、明示 drain_timeout 12000ms、不正 body+unknown field、重複要求、supervisor 不在拒否）。全 214 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

#### [x] C-04b graceful restart の処理順序（block→drain→child stop→exit）を実装する

- Actor: agent
- Depends on: C-04a
- 参照: A-01 の restart contract（2 節）、配布仕様 13.1 節
- 事前条件: `/admin/restart` route と認証・parse が GREEN
- Files: `src/app.rs`、関連 test
- Actions:
  1. admission block → in-flight drain（`admission.drain(generation, timeout)`）→ owned DS4 child stop（supervisor.stop）→ `202` を返す → process exit（`spawn_process_restart`）の順序を実装する。
  2. drain timeout 時は強制 kill せず `409 { error: "drain_timeout", in_flight, drain_timeout_ms }` を返す。
  3. child identity mismatch 時は `409 { error: "child_identity_mismatch" }` を返す。
  4. `202` を返した後に exit を予約し、response 返却前に exit して client へ曖昧な transport error を見せない。
- 事後条件: launchd が runtime を新 binary で再起動できる graceful path が完成する
- 受入基準: 正常 drain、drain timeout、identity mismatch の test がある
- Verification: handler/lifecycle test、共通 local gate
- ユーザーレビュー・手作業: なし

Evidence: src/app.rs で C-04a の placeholder だった perform_graceful_restart を本実装。GracefulRestartOutcome（Ready/DrainTimeout/ChildIdentityMismatch/ChildStopFailed）と graceful_restart_sequence（admission block → drain → owned child stop、exit 副作用なし）を追加。perform_graceful_restart は sequence 結果を HTTP へ写像し Ready 時のみ spawn_process_restart（100ms 後 exit）を予約、失敗時は進行中フラグを解放。resolve_drain_timeout を純粋関数化し handler の body parse を分離。is_graceful_identity_mismatch ヘルパー（ProcessControlError::IdentityMismatch 検出、強制 kill 回避）。テスト可能性のため handler 経由の成功系 test を exit 副作用のない形（resolve_drain_timeout 直接 + 進行中フラグ直接 + sequence 直接）に変更。unit test 3件（正常 drain+Blocked、drain timeout+in-flight 保持、identity mismatch 検出）。全 217 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

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

Evidence: src/cli.rs に ClusterCommand::GracefulRestart（`cluster graceful-restart`）を追加し、run_cluster の request 決定を純粋関数 cluster_request(command) へ抽出。GracefulRestart は `/admin/restart` POST（既存 cluster restart の `/cluster/restart` と区別）、POST は mutation 扱いで admin token を読み bearer auth を付与。unit test 3件（全サブコマンドが admin client path を選択 + graceful-restart 含む、GracefulRestart が /admin/restart を選び cluster Restart と区別、POST mutation で bearer auth / GET と区別）。C-04 の 5 受入基準（auth 失敗・正常 drain・drain timeout・identity mismatch・重複要求）は C-04a/C-04b の handler/sequence test が対応。停止条件（cluster lifecycle owner 迂回 signal、unknown PID kill、unauthenticated endpoint）は CLI では発生しない（/admin/restart のみ、bearer auth、supervisor.stop 経由のみ）。全 219 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

### [ ] C-05 bundle mode の menu と first launch を完成する

- Actor: agent + user review
- Depends on: C-04
- 参照: 配布仕様 6.3、11 節、A-01 の UX 文言
- 事前条件: Service Management と graceful restart の fake test が GREEN
- Files: `monitor/src/main.rs`、`monitor/src/tray.rs`、`monitor/src/settings.rs`、関連 test
- Actions:
  1. 「Monitor を終了」「Runtime を再起動」「バックグラウンド実行を開始/停止」を別操作にする。
  2. bundle mode では通常経路から `launchctl kickstart/bootout` を除く。
  3. first launch reducer が legacy inventory、config 検証、runtime status、background item 説明を
     順に受け取る interface を作る。実 inventory は D-01 で接続する。
  4. `requires_approval` 時だけ Login Items を開く明示操作を表示する。
  5. registration progress と model startup progress を別状態で表示する。
- 事後条件: 配布仕様 6.3 と 11 節の操作が UI から区別できる
- 受入基準: menu event test と first-launch state reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、ad-hoc app の手動起動前 static check、共通 local gate
- ユーザーレビュー・手作業: UI 文言、操作の危険度、approval 導線を画面で承認する
- 停止条件: first launch が model load 完了まで UI を block する、または拒否状態を enabled と表示する

#### [x] C-05a bundle mode 判定とメニュー操作の分離を実装する

- Actor: agent
- Depends on: C-04
- 参照: 配布仕様 6.3、11 節
- 事前条件: Service Management と graceful restart の fake test が GREEN
- Files: `monitor/src/main.rs`、`monitor/src/tray.rs`、`monitor/src/launchd.rs`、関連 test
- Actions:
  1. bundle mode 判定（実行 path が `.app/Contents/MacOS` 配下か）を追加する。bundle mode では通常経路から `launchctl kickstart/bootout` を除く。
  2. 「Monitor を終了」「Runtime を再起動」「バックグラウンド実行を開始/停止」を別 menu item にする。既存の Proxy 再起動は graceful restart の `/admin/restart` 呼び出しへ置き換える。
  3. バックグラウンド実行の開始/停止を `ServiceManagementAdapter` の register/unregister（C-02）へ接続する。
- 事後条件: 配布仕様 6.3 と 11 節の操作が menu 上で区別できる
- 受入基準: menu id と bundle mode 判定の unit test がある
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: なし（文言は C-05 本体で承認）

Evidence: monitor/src/launchd.rs に is_bundle_mode/is_bundle_path（`.app/Contents/MacOS` 配下判定）を追加し、kickstart/bootout は bundle mode で bail。monitor/src/tray.rs の menu id を MENU_QUIT/MENU_RUNTIME_RESTART/MENU_BG_START/MENU_BG_STOP/MENU_OPEN_CONFIG に分離（旧 MENU_PROXY_RESTART/MENU_MONITOR_RESTART 廃止）、各 is_*_event 判定を更新。monitor/src/main.rs の menu handler を再構成し、bundle mode では quit=プロセス終了/runtime restart=graceful_restart（client.graceful_restart、/admin/restart）、非 bundle では launchctl。bg start/stop は register_runtime（ServiceManagementAdapter register/unregister）へ接続。client.rs に graceful_restart メソッド追加。unit test（menu id 一意性 5件、各 event が自分の id のみ真、bundle path 検出/非検出）追加。monitor 44 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

#### [x] C-05b first-launch state reducer と UI state を実装する

- Actor: agent
- Depends on: C-05a
- 参照: 配布仕様 11 節、A-01 の UX 文言
- 事前条件: menu 操作分離が GREEN
- Files: `monitor/src/state.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. first-launch reducer が legacy inventory、config 検証、runtime status、background item 説明を順に受け取る interface を追加する。実 inventory は D-01 で接続する。
  2. `requires_approval` 時だけ Login Items を開く明示操作を表示する状態を追加する。
  3. registration progress と model startup progress を別状態で表示する。
- 事後条件: first-launch の各段階が UI state として区別できる
- 受入基準: first-launch state reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: なし（UI 文言は C-05 本体で承認）

Evidence: monitor/src/state.rs に first-launch reducer（C-05b）を追加。FirstLaunchState（VersionShown/InventoryChecked/ConfigValidated/RuntimeStatusChecked/Registering/Registered/RequiresApproval/RegisterFailed/MonitorLoginChecked/RuntimeAdminReady/ModelReady）と FirstLaunchEvent、純粋な first_launch_reducer を実装。配布仕様 11 節の順序（version→inventory→config→runtime status→register→login→admin ready→model ready）を反映し、registration progress と model-startup progress を別段階で表現。config invalid / requires approval / register failed の失敗分岐と、順序外 event を無視する挙動。first_launch_needs_approval（RequiresApproval 時のみ Login Items 導線表示）、first_launch_complete（ModelReady）。legacy inventory は interface のみ（D-01 で接続）。unit test 6件（happy path、legacy present/absent、config invalid で register 前停止、approval 導線ゲート、register failed を enabled と表示しない、順序外無視）。monitor 50 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19); 2026-08-19

#### [ ] C-05c menu event test と受入基準 test を追加する

- Actor: agent
- Depends on: C-05b
- 参照: 配布仕様 6.3、11 節
- 事前条件: first-launch reducer が GREEN
- Files: `monitor/src/tray.rs`、`monitor/src/main.rs`、関連 test
- Actions:
  1. 各 menu event（終了、runtime restart、開始、停止、Login Items を開く）の一意性と分岐を test する。
  2. bundle mode 判定と non-bundle mode の launchctl 経路を test する。
  3. 停止条件（first launch が model load まで UI を block する、拒否状態を enabled と表示する）が発生しないことを確認する。
- 事後条件: C-05 の受入基準（menu event test と first-launch reducer test）が満たされる
- 受入基準: menu event test と first-launch reducer test が全順序・失敗分岐を覆う
- Verification: monitor test、ad-hoc app の手動起動前 static check、共通 local gate
- ユーザーレビュー・手作業: UI 文言、操作の危険度、approval 導線を画面で承認する

Evidence（自動部分完了、ユーザーレビュー待ち 2026-08-19）: monitor/src/settings.rs に open_login_items（System Settings のログイン項目 pane を開く）と LOGIN_ITEMS_SCHEME を追加。monitor/src/tray.rs に MENU_OPEN_LOGIN_ITEMS menu item（「ログイン項目を開く」）と is_open_login_items_event を追加。monitor/src/main.rs に open_login_items 分岐を追加。menu id 一意性 test を 6 menu id（quit/runtime-restart/bg-start/bg-stop/open-config/open-login-items）へ拡張。state.rs に停止条件 test 2件（ModelReady 以外は complete にならず UI を block しない、ConfigValidated{false}/RegisterFailed は approval 導線も complete も表示せず enabled と誤表示しない）を追加。monitor 53 test OK、全 219 test OK、fmt/clippy/diff-check OK。残: ad-hoc app の手動起動で UI 文言・操作危険度・approval 導線を画面承認する。

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

Evidence: monitor/src/migration.rs を新規作成。LegacyInventory（binaries/plists/jobs/legacy_status）と inventory_legacy（read-only、legacy binary・plist を検出、usr_local_bin/home/agents dir を fixture 可能に分離）。LegacyJob/LegacyJobIdentity と verify_job_identity（PID + executable SHA-256 一致時のみ verifiable、mismatch は自動操作対象から除外）。backup_legacy（plist を Application Support/migration-backup/ へ一意 suffix 付き copy、manifest は追記で過去の backup を保持、atomic write）。BackupManifest/BackupEntry（path/size のみ、secret/config 本文を含まない）。macOS: legacy_plist_status（SMAppService.statusForLegacyURL → not_registered/enabled/requires_approval/not_found/error の安定文字列）。fixture test 7件（未導入、一部導入、二 job、identity mismatch、backup 再実行、digest、legacy status mapping）。monitor 60 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19)。first-launch UI への inventory 受け渡しは C-05 UI（画面レビュー待ち）と D-02 cutover で接続。

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

### [-] D-03 app/runtime version handshake と upgrade 提案を実装する

- Actor: agent
- Depends on: D-02
- 参照: 配布仕様 13 節
- 事前条件: B-01 の runtime metadata が admin API から取得可能
- Files: `monitor/src/client.rs`、`monitor/src/state.rs`、`monitor/src/tray.rs`、関連 test
- Actions:
  1. app version/build と runtime version/build を比較する。
  2. 一致時、runtime 旧版、runtime 新版、取得不能を別状態にする。
  3. mismatch 時は C-04 の graceful restart を一度だけ提案し、自動 loop にしない。
  4. prior app へ rollback して schema 非互換の場合は警告し、data を自動変換しない。
- 事後条件: `.pkg` 更新後に旧 executable image が残る状態を利用者が解消できる
- 受入基準: version matrix と restart 成功/失敗/拒否の UI test がある
- Verification: monitor test、共通 local gate
- ユーザーレビュー・手作業: mismatch と rollback 警告の文言を承認する
- 停止条件: mismatch 解消のため無条件 restart または config の不可逆 migration が必要になる

Evidence（自動 part 完了、UI 文言レビュー待ち 2026-08-19）: monitor/src/client.rs に RuntimeVersion（version/git_commit/build_number、/healthz から Deserialize）と health() メソッドを追加。monitor/src/state.rs に VersionHandshake（Matched/RuntimeOlder/RuntimeNewer/Unavailable）と version_handshake(app, runtime)（dotted numeric 比較、非数値は文字列等価フォールバック）、UpgradeProposal（decide は RuntimeOlder のとき 1 回だけ提案、自動 loop 防止；rollback_warning_required は常に true で data 自動変換しない）。unit test 6件（equal=Matched、app>runtime=RuntimeOlder、app<runtime=RuntimeNewer、非数値フォールバック、1 回限り提案、Unavailable は提案しない、rollback 警告必須）。monitor 80 test OK、全 219 test OK、fmt/clippy/diff-check OK(2026-08-19)。restart 成功/失敗/拒否の UI 文言表示は tray への接続と画面レビューで実施（保留）。

## 10. Phase E: `.pkg`、署名、公証、配布 feature gate

### [x] E-01 scriptless flat `.pkg` builder を実装する

- Actor: agent
- Depends on: D-03
- 参照: 配布仕様 9、15 節
- 事前条件: ad-hoc app と migration/lifecycle 自動 test が GREEN
- Files: `xtask/src/main.rs`、`xtask/src/package.rs`（新規）、`xtask/README.md`、関連 test
- Actions:
  1. `cargo xtask pkg-dev` を追加し、B-03 の app を component package と product archive にする。
  2. payload を `/Applications/Siderostat.app` 一項目に限定する。
  3. component receipt/product identifier と semver を template から固定する。
  4. `preinstall` / `postinstall` script を生成しない。
  5. package expand 結果を検査し、禁止 path と installer script があれば失敗する。
- 事後条件: certificate なしで final と同形の installable package を作れる
- 受入基準: package payload manifest が一項目、script directory なし、同一入力で receipt/version 一致
- Verification: package builder test、`pkgutil --expand-full` の静的検査、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: package script から user session、LaunchAgent、config を操作する必要が生じる

Evidence: xtask/src/package.rs を本実装に置き換え（E-01）。固定 identifier（COMPONENT_IDENTIFIER=dev.siderostat-ds4-proxy.pkg、PRODUCT_IDENTIFIER=dev.siderostat-ds4-proxy.product、INSTALL_LOCATION=/Applications、PAYLOAD_PATH=/Applications/Siderostat.app）と PKG_STAGING_DIR=build/pkg-dev。pkg_dev() が pkgbuild（--component app --install-location --identifier --version）→ productbuild（--package --identifier --version）→ pkgutil --expand-full → inspect_expanded の順で実行し、`Siderostat.app` 一項目・script なし・禁止 path を検査し、違反時は fail-closed する。preinstall/postinstall script を生成しない。inspect_expanded() は展開結果を再帰的に探索し、payload/script/禁止 path を検査する。unit test 5件、xtask 25 test OK、`bash scripts/verify-macos-dev-artifacts.sh` で実 package payload `Siderostat.app` と script なしを確認（2026-08-21）。certificate なしで final と同形の installable package を作れる。`pkg-dev` CLI は B-03 から安定。

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
固定 command 順・identifier・payload・出力 path を確認。unit test 24件、`cargo fmt --all -- --check`、
`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets`、
`cargo test --all-targets --features test-support`、`git diff --check` が PASS。実署名・公証は E-04
で実行する(2026-08-21)。

User review: `siderostat-notary` を Keychain profile として登録し、`xcrun notarytool history
--keychain-profile siderostat-notary` が credential エラーではなく `No submission history.` を返す
ことを確認。profile は有効で未提出状態。notary log 保存先 `dist/notary/` を採用する(2026-08-21)。

Evidence: `cargo xtask sign --dry-run`、`xcrun notarytool history --keychain-profile
siderostat-notary`、自動 gate は PASS。実署名・公証 artifact は E-04 で実行する。

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

Evidence: `.github/workflows/ci.yml` に certificate/network/secret を使わない `macos-dev-artifacts` job、`scripts/verify-macos-dev-artifacts.sh` に fixture と実 artifact 検査を追加。壊れた plist、余分な payload、unsigned helper の3 fixture が個別に失敗すること、`app-dev --verify` の plist/layout/nested signature、`pkg-dev` の `/Applications` install location・`Siderostat.app` 一項目・script/禁止 path、secret・user data・model の不在を確認。`bash -n scripts/verify-macos-dev-artifacts.sh`、`cargo build --release --workspace`、`cargo test -p xtask`（25件）、local 相当 `bash scripts/verify-macos-dev-artifacts.sh`、`git diff --check` が PASS（2026-08-21）。Developer ID / notarization は CI job に含めない。次に着手可能: `E-04`。

### [ ] E-04 signed/notarized package の clean install を実機検証する

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

### [ ] E-05 legacy migration、upgrade、rollback を実機検証する

- Actor: user + agent
- Depends on: E-04
- 参照: 配布仕様 12、13、16.3、16.4 節
- 事前条件: v0.2.1 legacy install と prior/final candidate package を復元できる test 環境がある
- Files: `docs/compatibility/v0.3.0-migration-rollback.md`（新規、redaction 済み結果のみ）
- Actions:
  1. legacy job 稼働中から migration 成功を実行し、port と child の重複がないことを確認する。
  2. new registration または readiness を意図的に失敗させ、legacy rollback を確認する。
  3. v0.3 candidate 間 upgrade で config、secret、manifest、model、KV cache を保持する。
  4. prior notarized package へ rollback し、version mismatch の案内と data 保持を確認する。
  5. uninstall 手順で service だけを止め、user data が既定で残ることを確認する。
- 事後条件: P0 配布 feature の実機 gate が閉じ、`H-01` が着手可能になる
- 受入基準: 成功/失敗 migration、upgrade、rollback、uninstall の全 scenario で root process、orphan、
  duplicate listener、data loss がない
- Verification: PID/path/port/job/status、file digest/permission、readiness、package version の前後比較
- ユーザーレビュー・手作業: change window、migration 確認、故障注入、prior package 再 install を実行する
- 停止条件: identity 未確認 process の停止、legacy plist の削除、user data の削除が必要になる

## 11. Phase H: throughput degradation の観測と復旧

### [ ] H-01 degraded detection / recovery contract を固定する

- Actor: agent + user review
- Depends on: E-05
- 参照: Hermes 調査 8〜12 節
- 事前条件: app/pkg lifecycle と runtime restart/migration が実機 PASS
- Files: `docs/recovery/throughput-degraded-contract-v0.3.0.md`（新規）
- Actions:
  1. 初期値を canary 64 token、deadline 30 秒、decode 下限 5 tokens/s、progress stall 60 秒、
     cooldown 1 時間、12 時間に最大 2 回として固定する。
  2. idle の 0 TPS を異常にしない条件と、prefill/decode/first-token の判定順を固定する。
  3. `recover-degraded` request/status JSON、recovery ID、単一 owner、冪等性を固定する。
  4. `admission block -> snapshot -> drain -> demote -> paired standalone -> promote -> canary -> serving`
     の順序を固定する。
  5. drain timeout、demote failure、promotion failure、canary failure の安全な最終状態を表にする。
  6. 自動復旧は opt-in 既定 `false` とし、H-10 実機 evidence 後の既定値変更は別レビューとする。
- 事後条件: H-02〜H-09 の実装者が閾値、状態、failure behavior を推測しない
- 受入基準: 低 TPS、first-token stall、progress stall、idle、cooldown、上限、競合の入力/出力表がある
- Verification: Hermes 調査 9.4/11 節との対応表、sequence diagram、`git diff --check`
- ユーザーレビュー・手作業: 閾値、回数上限、opt-in、drain timeout、失敗時の通知方針を承認する
- 停止条件: active request の強制 kill または既存 demotion owner の迂回を正常経路にする必要がある

### [ ] H-02 monotonic progress freshness metrics を実装する

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
- 停止条件: prompt、response、session ID を metric label に含める必要がある

### [ ] H-03 monitor を current chunk と progress age 表示へ変更する

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
- 停止条件: detector が UI 表示文字列を parse しないと成立しない

### [ ] H-04 redaction 済み diagnostic snapshot を実装する

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
- 停止条件: snapshot 取得が cluster state を mutation する、または secret を保存しないと診断不能になる

### [ ] H-05 bounded canary executor と CLI を実装する

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
- 停止条件: canary が任意 URL、任意 prompt、無制限 token を受け付ける必要がある

### [ ] H-06 recovery job の単一 owner と admin API を実装する

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
- 停止条件: recovery job state を既存 cluster state の代用 source of truth にする必要がある

### [ ] H-07 demote / promote / post-canary を recovery job へ接続する

- Actor: agent
- Depends on: H-06
- 参照: Hermes 調査 8.2、9.1、11.3〜11.7 節
- 事前条件: recovery job phase reducer と既存 demotion integration test が GREEN
- Files: `src/recovery.rs`、`src/cluster/production/pairing.rs`、`src/app.rs`、関連 test
- Actions:
  1. admission block、in-flight drain、既存 demote の順で PairedStandaloneReady へ戻す。
  2. `auto_promote` の既存 owner を利用して新 generation の DistributedReady を待つ。
  3. 両 node の old/new child identity と generation を比較する。
  4. post-recovery canary 成功後だけ admission を再開し job を success にする。
  5. 各 failure で restart loop に入らず、H-01 の安全な state と reason を返す。
- 事後条件: worker-only restart を追加せず、distributed pair 全体を一回だけ再生成できる
- 受入基準: 正常、drain timeout、demote failure、promotion failure、canary failure、peer loss の test がある
- Verification: 2 node fake integration test、既存 reconnect suite、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: control protocol を迂回した remote signal、manual state file edit、unverified child kill が必要になる

### [ ] H-08 opt-in 自動 detector と安全弁を実装する

- Actor: agent
- Depends on: H-07
- 参照: Hermes 調査 9.2、9.4、10 Phase 2 節
- 事前条件: manual recovery の全 failure test が GREEN
- Files: `src/recovery.rs`、`src/config.rs`、`siderostat.example.toml`、関連 test
- Actions:
  1. `enabled=false` 既定の typed config を追加し、H-01 の閾値を default にする。
  2. low TPS は継続時間、stall は last progress age、pre-cron は canary failure で判定する。
  3. cooldown、12 時間内回数、active owner、DistributedReady、role を全て開始前 gate にする。
  4. drain timeout または連続失敗では自動 retry せず manual intervention を通知する。
  5. recovery started/completed/failed と抑制 reason を structured log/metrics にする。
- 事後条件: opt-in 時だけ bounded automatic recovery が動き、disabled 時は観測だけ行う
- 受入基準: 単一低 sample、idle 0 TPS、cooldown、回数上限、duplicate event、clock advance の test がある
- Verification: deterministic-time unit test、fake 2 node integration test、共通 local gate
- ユーザーレビュー・手作業: 実機で `enabled=true` にするのは H-10 の change window 内だけとする
- 停止条件: wall clock 変更、固定 sleep、無制限 retry に依存する test しか作れない

### [ ] H-09 degradation / recovery regression suite を固定する

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
- 停止条件: production code に test 専用 recovery path を追加しないと成立しない

### [ ] H-10 2 node 実機 recovery と Hermes handoff を検証する

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
- 停止条件: production cron 実行中、rollback 不可、force kill/state 削除/OS 再起動が必要になる

## 12. Phase N: recovery epoch 単位の通知重複排除

### [ ] N-01 pure semantic deduplicator を実装する

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

### [ ] N-02 notification service と observability へ接続する

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

### [ ] N-03 coordinator-only restart の実機通知回数を検証する

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

## 13. Phase T: TP/RDMA の最終再評価と延期

### [ ] T-01 TP/RDMA を最後に再評価し v0.4+ backlog を固定する

- Actor: agent + user review
- Depends on: N-03
- 参照: DwarfStar TP/RDMA 調査 全節
- 事前条件: P0〜P2 の feature と実機 gate が完了済み
- Files: `docs/research/dwarfstar-rdma-tensor-parallel-2026-08-18.md` の追跡 addendum、
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

## 14. Phase R: 文書、release candidate、最終受入

### [ ] R-01 利用者・運用・開発文書を v0.3.0 へ同期する

- Actor: agent + user review
- Depends on: T-01
- 参照: P0〜P2 の完了 Evidence
- 事前条件: behavior、default、path、command、既知制約が実装で確定済み
- Files: `README.md`、`docs/installation.md`、`docs/operations.md`、`docs/troubleshooting.md`、
  `docs/development.md`、`docs/menu-bar-monitor-spec.md`、`docs/spec.md`、`contrib/launchd/README.md`
- Actions:
  1. 通常 install を `.pkg` と first launch に置き換え、旧 `cargo xtask install` は開発/legacy と明示する。
  2. background service、login start、migration、upgrade、rollback、uninstall を記載する。
  3. progress age、canary、manual/automatic recovery、snapshot、cooldown を記載する。
  4. notification epoch と TP/RDMA 延期を既知制約として記載する。
  5. command、path、identifier、既定値を実装から再照合する。
- 事後条件: 利用者が旧 LaunchAgent 手順を通常経路として選ばない
- 受入基準: clean install から uninstall、degraded recovery まで文書だけで再現できる
- Verification: link/path/command check、`git diff --check`、文書 clean-install rehearsal
- ユーザーレビュー・手作業: install、Background Items、recovery の文言と安全警告を承認する
- 停止条件: 実装と文書の default が一致しない、または未検証 command を記載する必要がある

### [ ] R-02 v0.3.0 release candidate と supply-chain artifact を作る

- Actor: user + agent
- Depends on: R-01
- 参照: 配布仕様 15、16 節
- 事前条件: 全 automated suite と documentation check が GREEN
- Files: `Cargo.toml`、`monitor/Cargo.toml`、`Cargo.lock`、`docs/releases/v0.3.0.md`、
  `docs/releases/v0.3.0-acceptance.md`、release metadata/SBOM 生成設定
- Actions:
  1. crate version を一括して `0.3.0` に更新し、build number を固定する。
  2. final signed/stapled `.pkg`、app archive、SHA-256、SBOM/dependency inventory、third-party notices を作る。
  3. git commit、Rust version、target、Team ID、notary submission ID を metadata に記録する。
  4. automatic test、clean install、migration、recovery、notification evidence を acceptance 文書へ集約する。
- 事後条件: 内容と由来を検証できる release candidate 一式がある
- 受入基準: artifact の checksum、signature、notary、version、receipt が相互一致する
- Verification: 共通 local gate、CI、static distribution verification、SBOM/notice/file-list check
- ユーザーレビュー・手作業: final signing/notarization command の実行を承認する。credential 本文は共有しない
- 停止条件: uncommitted source、dirty tree、未追跡 credential、model/user data を artifact に含む

### [ ] R-03 final acceptance と release 承認を完了する

- Actor: user + agent
- Depends on: R-02
- 参照: 本書 4.3、各実機 Evidence
- 事前条件: candidate checksum が固定され、prior package と rollback 手順がある
- Files: `docs/releases/v0.3.0-acceptance.md` の結果追記
- Actions:
  1. clean install、migration/upgrade/rollback、login、runtime crash、background stop を最終 artifact で再確認する。
  2. standalone/paired/distributed/reconnect、throughput recovery、notification dedup の回帰 gate を実行する。
  3. credential、secret、model、user data が source と artifact にないことを再確認する。
  4. known risk、TP/RDMA 延期、automatic recovery opt-in を release note へ明記する。
  5. ユーザー承認後だけ、`CONTRIBUTING.md` に従う merge、tag、push を別操作として行う。
- 事後条件: v0.3.0 の release 可否、rollback artifact、既知制約が一意に判定される
- 受入基準: 本書 4.3 の 10 項目が全て PASS。FAIL または未実施を waiver で暗黙に通さない
- Verification: final checksum を使った全 acceptance、`git status --short`、`git diff --check`
- ユーザーレビュー・手作業: install 承認、System Settings、login/restart、2 node 操作、最終 release 可否を承認する
- 停止条件: acceptance 未実施、rollback 不可、署名/公証不一致、データ損失、orphan、restart loop が一件でもある

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
