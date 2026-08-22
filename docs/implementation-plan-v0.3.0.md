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
- ds4-server の実行形態と Siderostat の正規用語:
  [`ds4-mode-taxonomy.md`](ds4-mode-taxonomy.md)

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

## 10. Phase E: `.pkg`、署名、公証、配布 feature gate

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
  4. bundle replacement 前に既存 Monitor だけを終了する `preinstall` を生成する。
  5. package expand 結果を検査し、想定外の script と禁止 path があれば失敗する。
- 事後条件: certificate なしで final と同形の installable package を作れる
- 受入基準: package payload manifest が一項目、制御された `preinstall` 一項目、同一入力で receipt/version 一致
- Verification: package builder test、`pkgutil --expand-full` の静的検査、共通 local gate
- ユーザーレビュー・手作業: なし
- 停止条件: package script から user session、LaunchAgent、config を操作する必要が生じる

Evidence: `xtask/src/package.rs` を本実装に置き換え（E-01）。固定 identifier（COMPONENT_IDENTIFIER=dev.siderostat-ds4-proxy.pkg、PRODUCT_IDENTIFIER=dev.siderostat-ds4-proxy.product、INSTALL_LOCATION=/Applications、PAYLOAD_PATH=/Applications/Siderostat.app）と PKG_STAGING_DIR=build/pkg-dev。`pkg_dev()` が app を一時 payload root へ `ditto` し、BundleIsRelocatable=false の component plist と、既存 Monitor の完全一致した実行パスだけを SIGTERM（最大10秒）→必要時のみ SIGKILL する `preinstall` を指定した `pkgbuild`（`--root --component-plist --scripts --install-location --identifier --version`）→ `productbuild`（`--package --identifier --version`）→ `pkgutil --expand-full` → `inspect_expanded` の順で実行する。既存 bundle identifier による `/Applications` 外への relocation を抑止し、`Siderostat.app` 一項目・制御された `preinstall` 一項目・禁止 path を検査し、違反時は fail-closed する。runtime、`ds4-server`、LaunchAgent、設定、secret、ユーザーデータは installer script の対象外とする。`inspect_expanded()` は展開結果を再帰的に探索し、payload/script/禁止 path を検査する。unit test 7件、xtask package test 7件、`cargo clippy --workspace --all-targets -- -D warnings`、`git diff --check` が PASS（2026-08-22）。certificate なしで final と同形の installable package を作れる。`pkg-dev` CLI は B-03 から安定。

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
