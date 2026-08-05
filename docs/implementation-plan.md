# DS4 Smart Proxy 実装計画

## 1. 文書の目的と寿命

本書は [`spec.md`](spec.md) を実装可能な作業単位へ分解し、作業順序、依存関係、検証、evidence、rollbackを管理する。低い推論能力の実行者でも、未記載の設計判断を極力行わずに1 taskずつ進められることを目的とする。

本書はmode-aware architectureへの刷新期間中に使用する時限的な計画文書である。Release完了後は必要な記録をrelease recordへ移し、本書をarchiveまたは削除できる。

恒久文書の正本：

| 対象 | 正本 |
|---|---|
| 製品behavior、protocol、acceptance criteria | [`spec.md`](spec.md) |
| Git、branch、commit、review | [`../CONTRIBUTING.md`](../CONTRIBUTING.md) |
| Agent固有の必須指示 | [`../AGENTS.md`](../AGENTS.md) |
| 今回の作業順序、task状態、evidence | 本書 |

本書と仕様書が矛盾する場合は仕様書を優先し、実装を止めて本書を修正する。実装で仕様書を暗黙に変更してはならない。

## 2. 状態表記

Task headingのcheckboxを唯一の進捗状態とする。

- `[ ]`：未着手
- `[-]`：着手中
- `[x]`：完了。Verificationとevidenceが必要
- `[!]`：blocked。理由、再開条件、確認済み事項が必要

実装を始めるときだけ `[ ]` を `[-]` に変更する。Codeを書いただけでは `[x]` にしない。Task固有testと共通gateが成功し、commitまたは保存済みartifactをEvidence欄へ記録してから `[x]` にする。

## 3. 実行者が毎回守る手順

### 3.1 Task選択

1. 本書を開く。
2. `[ ]` のtaskから、`Depends on` がすべて `[x]` の最初のtaskを1つ選ぶ。
3. `Actor: operator` のtaskは、明示的な依頼なしに実行しない。
4. 選択taskだけを `[-]` にする。
5. Taskが参照する仕様書sectionと対象fileを読む。

複数taskを同じchangeで実装しない。ただし、compile維持に必要な機械的変更は同じtaskへ含めてよい。その場合はEvidenceへ理由を書く。

### 3.2 Preflight

各task開始前に次を実行する。

```text
git status --short
git branch --show-current
git diff --check
```

- Userの未commit変更を破棄、上書き、整形しない。
- 想定外の変更が対象fileと重なる場合はtaskを `[!]` にせず、まず作業を止めて確認する。
- Branchが `CONTRIBUTING.md` の規則に反する場合はcodeを変更しない。
- Secret、model、GGUF、KV cache、runtime stateをrepositoryへ追加しない。

### 3.3 実装

- `Files` に列挙されたfileだけを原則変更する。
- 別fileが必要になった場合は先に本書の当該taskへ追加する。
- 新しいdependencyはtaskで明示されている場合だけ追加する。
- Shell command textをparseしてmacOS状態を判断しない。
- Network設定を自動変更しない。
- Unknown processへsignalを送らない。
- 未実装機能をREADMEへ利用可能と書かない。

### 3.4 Verification

Task固有のVerificationを先に実行し、続けて影響範囲に応じた共通gateを実行する。

共通local gate：

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
git diff --check
```

Dependencyやplatform APIを変更した場合は `cargo check --all-targets --all-features` も実行する。Hardwareが必要なtestを実行できない場合は成功扱いにせず `[!]` とし、実行済みの範囲を記録する。

Fake DS4を使うtaskでは、共通gateに加えて次を実行する。

```text
cargo test --all-targets --features test-support
```

### 3.5 Evidence

完了taskの直下に次の形式で1行追加する。

```text
Evidence: <commit SHA またはartifact path>; <実行したtest>; <日付 YYYY-MM-DD>
```

失敗を隠して成功したtestだけを記録しない。Retry後に成功した場合は最初のfailure原因もcommit本文またはartifactへ残す。

### 3.6 共通停止条件

次の場合は推測で進めず停止する。

- 仕様書にないprotocol、port、timeout、role electionが必要。
- DS4 wire/log/CLIが仕様書のverified baselineと異なる。
- HMAC、secret permission、source interface検証を弱める必要がある。
- Unknown childを停止しないとportを取得できない。
- Model digest、binary digest、layer splitが一致しない。
- Testを通すためにtest削除、assertion緩和、timeout無制限化が必要。
- User data、KV、legacy SQLiteを削除する必要がある。
- `main`、`legacy/*`、公開済みtagへのforce pushが必要。

## 4. 現在のrepository baseline

Plan作成時点：

| 項目 | 値 |
|---|---|
| Package | `ds4-smart-proxy 0.1.0` |
| Rust | edition 2024 |
| Current load-balancer candidate | commit `b66ba1c` |
| Existing integration test directory | なし |
| Existing crate shape | `src/main.rs` をrootとするbinary crate |
| Current config | `backends`、`routing`、`affinity`、`heartbeat`を含むlegacy schema |
| Target config | `docs/spec.md` 第22章のschema v2 |

Legacy moduleと移行先：

| 現行file | 現在の責務 | 移行先または最終処置 |
|---|---|---|
| `src/routing.rs` | 複数backend選択 | `src/target.rs` へ置換後削除 |
| `src/backend.rs` | backend registry/health | local child/target readinessへ分解後削除 |
| `src/affinity.rs` | session/prefix affinity | request pathから除去後削除 |
| `src/persistence.rs` | affinity SQLite | `cluster/state_store.rs`とは別物。移行せず削除 |
| `src/heartbeat.rs` | backend heartbeat | local readinessとcluster leaseへ置換後削除 |
| `src/proxy.rs` | streaming/retry/routing | streaming primitiveを保持しtarget固定化 |
| `src/config.rs` | legacy config | schema v2へ置換 |
| `src/main.rs` | CLI/listener/admin | thin entrypointへ縮小 |
| `src/app.rs` | legacy state assembly | mode-aware component assemblyへ置換 |
| `src/metrics.rs` | legacy metrics | spec第26章へ置換 |

Target source treeは仕様書第30章を基準とする。Integration testを可能にするため `src/lib.rs` と `tests/` を追加してよい。

## 5. 成果物

| 成果物 | 完了条件 |
|---|---|
| Mode-aware proxy | 仕様書第9–12章と第33.1節を満たす |
| Thunderbolt peer discovery | 第13章と第33.2節を満たす |
| DS4 supervisor | 第14、20、21章を満たす |
| Distributed lifecycle | 第15–19章と第33.3節を満たす |
| Operations | 第23–28、35、36章を満たす |
| `README.md` | 旧load balancing/affinity説明が残っていない |
| `docs/installation.md` | Cleanな2-node環境をDistributedReadyまで構築できる |
| Configuration examples | 実parserとdoctorで成功する |
| Compatibility record | DS4 commit、digest、fixture、actual resultを記録する |

## 6. 依存関係

```text
T-01 -> T-02 -> T-03
                 `-> P0-01 -> P0-02
                                  |-> P0-03 --+
                                  |-> P0-04 --+-> P0-06
                                  `-> P0-05 --+

P1-01 -> P1-02 -> P1-03 -> P1-04 -> P1-05 -> P1-06 -> P1-07 -> P1-08
                                                                  |
                                                                  v
P2-01 -> P2-02 -> P2-03 -> P2-04 -> P2-05 -> P2-06 -> P2-07 -> P2-08
                                                                  |
                                                                  v
P3-01 -> P3-02 -> P3-03 -> P3-04 -> P3-05 -> P3-06
                                                    |-> P3-07 -----+
                                                    |              |
                                                    `-> P4-01 -> P4-02 -> P4-03
                                                        -> P4-04 -> P4-05 -> P4-06
                                                                       |  |
                                                                       +--+-> P4-07
                                                                               |
                                                                               v
P5-01 -> P5-02 -> P5-03 -> P5-04 -> P5-05 -> P5-06
                                                        |
                                                        v
P6-01 -> P6-02 -> P6-03 -> P6-04 -> P6-05
                                      |
                                      v
P7-01 -> P7-02 -> P7-03 -> P7-04
```

図は概要であり、各taskの `Depends on` を正とする。Phase内taskは原則直列で行う。P0-03とP0-04だけはP0-02完了後に並行可能だが、同じworktreeでは並行編集しない。P4-01はP3-06完了後に開始できるが、P4-07にはP3-07とP4-06の両方が必要である。

## 7. 詳細task list

### Transition: Git baseline

#### [x] T-01 Legacy baselineを検証する

- Actor: agent
- Depends on: なし
- Read: `CONTRIBUTING.md`、現行`README.md`
- Files: code変更なし。Evidenceは本taskへ追記
- Actions:
  1. Commit `b66ba1c` がload balancer系最終候補であることをlogで確認する。
  2. Dirty worktreeを別refへ保全できる状態か確認する。
  3. `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets`を実行する。
  4. Failureがあれば修正せず、commandと最初のerrorを記録する。
- Verification: 上記3 command
- Done when: `b66ba1c` のbaseline可否がevidence付きで確定
- Stop when: 現在の未commit変更を退避・保持できない

Evidence: `b66ba1c`; `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets`（sandbox内ではloopback bindが`PermissionDenied`、sandbox外retryで32件成功）; 2026-08-06

#### [x] T-02 Legacy refを作成する

- Actor: operator
- Depends on: T-01
- Files: Git refsのみ
- Actions:
  1. `legacy/load-balancer-v1` をtest済みcommitへ作成する。
  2. Annotated tag `load-balancer-v1-final` を同commitへ作成する。
  3. Local ref targetを確認する。
  4. 明示的なpush依頼がある場合だけremoteへpushする。
- Verification: branchとtagが同一commitを指す
- Done when: Local refが作成され、push有無がevidenceに記録
- Stop when: Baseline test未成功、または同名refが異なるcommitに存在

Evidence: local branch `legacy/load-balancer-v1` とannotated tag `load-balancer-v1-final`（ともに`b66ba1cff29da4b8dc2edd34870c62595ee1dfdd`、remote pushなし）; `git rev-parse`、`git rev-list -n 1`; 2026-08-06

#### [x] T-03 Rewrite integration branchへ作業を保全する

- Actor: operator
- Depends on: T-02
- Files: Git refs、現在の仕様・計画文書
- Actions:
  1. `rewrite/mode-aware` をlegacy最終commitから作成する。
  2. 現在の`AGENTS.md`、`CONTRIBUTING.md`、`docs/spec.md`、本書を保全する。
  3. Diffを確認して文書以外の意図しない変更がないことを確認する。
  4. 文書変更を1つのreview可能なcommitにする。
- Verification: `git diff --check`、commitから4文書が到達可能
- Done when: Rewrite branch上に文書baseline commitが存在
- Stop when: Untracked fileの所有者または含める範囲が不明

Evidence: `AGENTS.md`、`CONTRIBUTING.md`、`docs/spec.md`、`docs/implementation-plan.md`; `git diff --check`、4文書だけのstatus/diff確認、`rewrite/mode-aware`への到達性確認; 2026-08-06

#### [ ] T-04 Repository protectionを設定する

- Actor: operator
- Depends on: T-03
- Files: repository host settings
- Actions: `main` と `legacy/load-balancer-v1` に `CONTRIBUTING.md` のprotectionを設定
- Verification: Force push禁止とrequired checksをhost UI/APIで確認
- Done when: 設定のscreenshotまたはURLをevidenceに記録
- Stop when: Required CI自体が未作成。P7-01へ延期してよい

### Phase 0: Baselineとtest基盤

#### [x] P0-01 現行behavior fixtureを保存する

- Actor: agent
- Depends on: T-03
- Read: 現行`src/`、`README.md`、`docs/hermes-600s-failover.md`
- Files: `tests/fixtures/legacy/`、本書
- Actions:
  1. Config parse、public proxy streaming、admin readinessの既存test名を一覧化する。
  2. Legacy config exampleをfixtureとしてcopyする。
  3. 既存test結果を保存する。Prompt/body/secretは保存しない。
- Verification: `cargo test --all-targets`
- Done when: 後続migrationで退行比較できるfixtureが存在

Evidence: `tests/fixtures/legacy/README.md`、`tests/fixtures/legacy/ds4-smart-proxy.example.toml`; legacy exampleとの`cmp`、共通local gate、32 tests成功; 2026-08-06

#### [x] P0-02 Library/test harnessへ挙動不変で分離する

- Actor: agent
- Depends on: P0-01
- Files: `src/lib.rs`、`src/main.rs`、`src/app.rs`、`tests/support/`
- Actions:
  1. Reusable moduleを`src/lib.rs`から参照可能にする。
  2. `main.rs`をargument parse、logging、library起動だけへ縮小する。
  3. Existing unit testのlocationとassertionを変えない。
  4. Integration test用helper directoryを作る。Fake behaviorはまだ実装しない。
- Verification: 共通local gate。Migration前後の既存test数を比較
- Done when: Behavior差分なしでintegration testからcrateを利用可能
- Stop when: Public API化のためproduction型のsecurity invariantを弱める必要がある

Evidence: `src/lib.rs`、`tests/support/mod.rs`; `cargo check --all-targets --all-features`、共通local gate、移行前後とも32 tests成功; 2026-08-06

#### [x] P0-03 DS4 compatibility recordを作成する

- Actor: agent + operator for actual command
- Depends on: P0-02
- Read: 仕様書第14–17、20、36章
- Files: `docs/compatibility/ds4-b7e9f00.md`、`tests/fixtures/ds4/`
- Actions:
  1. DS4 source commit、binary SHA-256、model profileを記録するtemplateを作る。
  2. `--help`から使用optionを確認し、command outputをsanitizeして保存する。
  3. HELLO 40-byte schemaとrecognized log lineのfixtureを採取する。
  4. Q2、Q2-Q4、MXFP4、SSD streamingの確認済み/未確認を分ける。
- Verification: Fixture parser testを後続P4-03で利用可能な形式にする
- Done when: Unknown事項が空欄ではなく`未確認`と記録
- Stop when: DS4 commitが仕様書baselineと異なる。勝手にbaseline更新しない

Evidence: `docs/compatibility/ds4-b7e9f00.md`、`tests/fixtures/ds4/`; 69-byte synthetic frame確認、fixture SHA-256検証、共通local gate、32 tests成功; 2026-08-06

#### [x] P0-04 macOS network API spikeを完了する

- Actor: agent + operator for cable operation
- Depends on: P0-02
- Read: 仕様書第13、29章
- Files: `spikes/macos-network/`または`tests/fixtures/network/`、compatibility record
- Actions:
  1. System Configurationでservice enabledとDynamic Store notificationを読めることを確認する。
  2. `if_nametoindex("bridge0")` と `getifaddrs` を確認する。
  3. DNS-SD register/browseをinterface indexへ限定できることを確認する。
  4. Cable attach/detach時のevent順序、重複、delayを記録する。
  5. Binding dependencyを1つ選び、選定理由と未使用候補を記録する。
- Verification: Spike build、sanitized fixture、cable event実測
- Done when: Production moduleで使うAPI/bindingが1つに決定
- Stop when: Private driver classをcorrectness条件にしないと実現できない

Evidence: `spikes/macos-network/results-2026-08-06.md`; warning-free spike build、read-only snapshot、Cable detach/attach 2 cycles、Dynamic Store event timing/duplicate確認、bridge0限定Bonjour register/browse成功、共通local gate; 2026-08-06

#### [x] P0-05 Fake DS4 test serverを作る

- Actor: agent
- Depends on: P0-02
- Files: `Cargo.toml`、`src/bin/fake-ds4.rs`、`tests/support/`、`tests/fake_ds4.rs`、`tests/fixtures/ds4/`
- Actions:
  1. `test-support` featureでだけbuildされる`fake-ds4` binary targetを追加する。Release default buildへ含めない。
  2. `/v1/models` readinessを返す。
  3. SSEを100ms間隔で送る。
  4. Startup delay、mid-stream close、SIGTERM、exit codeを制御可能にする。
  5. Child argvをcaptureする。
  6. Prompt/bodyをlogしない。
- Verification: `cargo test --all-targets --features test-support`。Cancellation後にprocess/listenerが残らない
- Done when: P1以降が実DS4なしで再現可能

Evidence: `src/bin/fake-ds4.rs`、`tests/fake_ds4.rs`; `cargo check --all-targets --all-features`、共通local gate、`cargo test --all-targets --features test-support`（34 tests成功、SIGTERM後process/listener不在）; 2026-08-06

#### [x] P0-06 Phase 0 gateを閉じる

- Actor: agent
- Depends on: P0-03、P0-04、P0-05
- Files: 本書のEvidenceのみ
- Verification: 共通local gate、fixture checksum
- Done when: Phase 1がDS4/Networkの未確定APIに依存しない

Evidence: `tests/fixtures/legacy/`、`tests/fixtures/ds4/`、`spikes/macos-network/results-2026-08-06.md`; fixture SHA-256、legacy example `cmp`、共通local gate（32 tests）、test-support gate（34 tests）成功; 2026-08-06

### Phase 1: Mode-aware proxy core

#### [x] P1-01 Config schema v2の型を実装する

- Actor: agent
- Depends on: P0-06
- Read: 仕様書第19、22章
- Files: `src/config.rs`、config unit tests
- Actions:
  1. `proxy`、`cluster`、`cluster.security`、`cluster.policy`、`cluster.timeouts`、`cluster.discovery`、`ds4`、`ds4.standalone`、`ds4.mxfp4`、`logging`を型にする。
  2. `deny_unknown_fields`相当でunknown fieldを拒否する。
  3. Duration、address、pathはparse時に型へ変換する。
  4. Legacy fieldを黙って無視しない。
- Verification: Complete example parse、unknown/legacy rejection test
- Done when: 仕様書第22.2節のexampleがparse可能
- Stop when: 仕様書にdefaultがないfieldを推測する必要がある

Evidence: `src/config.rs`; schema v2 complete example parse、unknown nested field/legacy root field rejection、typed duration/address/path確認、共通local gate（35 tests）、test-support gate（37 tests）成功; 2026-08-06

#### [x] P1-02 Config validationとpath expansionを実装する

- Actor: agent
- Depends on: P1-01
- Files: `src/config.rs`、`src/error.rs`
- Actions: 仕様書第22.1、22.3、22.4節のvalidationを1項目1testで実装
- Verification: Port衝突、secret permission、model/KV分離、SSD option重複test
- Done when: Validation checklistがtest名と1対1対応
- Stop when: Testのため実model fileが必要。Temporary regular fileで代用する

Evidence: `src/config.rs`; schema/path/port/DNS-SD/duration/file/model/residency/KV/layer/secret/extra-arg validation 22 tests、temporary regular files使用、共通local gate（50 tests）、test-support gate（52 tests）成功; 2026-08-06

#### [x] P1-03 Target resolverを実装する

- Actor: agent
- Depends on: P1-02
- Read: 仕様書第9章
- Files: `src/target.rs`、`src/lib.rs`
- Actions:
  1. `StableMode`、`ClusterState`、`ProxyTarget`を定義する。
  2. 第9.2節のtableをpure functionとして実装する。
  3. Request内容、latency、sessionをinputに含めない。
- Verification: Table全rowとunknown roleのunit test
- Done when: 1 stateから複数targetを返すpathがない

Evidence: `src/target.rs`; stable table全6 row、全transition state、mode/state不整合、unknown role readiness test、共通local gate（54 tests）、test-support gate（56 tests）成功; 2026-08-06

#### [x] P1-04 Admission gateを実装する

- Actor: agent
- Depends on: P1-03
- Read: 仕様書第11、12、28章
- Files: `src/admission.rs`、unit tests
- Actions:
  1. `Serving/Draining/Blocked`を実装する。
  2. Permitをresponse EOF/error/client cancellationまで保持する。
  3. In-flight zero waitとtimeoutを実装する。
  4. Generation違いのackを拒否するhookを用意する。
- Verification: Race/cancellation/timeout unit test
- Done when: Cancellation storm後にin-flight=0

Evidence: `src/admission.rs`; serving/readiness/capacity、drain race、timeout、generation ack、128-task cancellation storm test、共通local gate（59 tests）、test-support gate（61 tests）成功; 2026-08-06

#### [x] P1-05 Proxyを単一target forwardingへ変更する

- Actor: agent
- Depends on: P1-04
- Read: 仕様書第10、11章
- Files: `src/proxy.rs`、proxy tests
- Actions:
  1. Existing streamingとhop-by-hop header処理を保持する。
  2. Backend selectionを`ProxyTarget`解決へ置換する。
  3. Alternate backend retryを削除する。
  4. Transition中は503 + `Retry-After`、connect failureは502を返す。
  5. Peer ingress hop headerを将来追加できる内部request contextを用意する。
- Verification: SSE timing、unknown path、body limit、cancel、no-retry test
- Done when: Request pathがaffinity/backend registryを参照しない

Evidence: `src/proxy.rs`、`src/lib.rs`; `ProxyTarget`固定解決、request/response streaming、hop-by-hop・内部cluster header除去、body累積上限、permitのEOF/error/drop保持、transition 503 + `Retry-After`、connect 502、alternate retryなしを実装。mode-aware proxy 8 tests、共通local gate（68 tests）、test-support gate（70 tests）、`cargo check --all-targets --all-features`、`cargo clippy --all-targets --all-features -- -D warnings`成功; 2026-08-06

#### [x] P1-06 App stateとbasic admin APIを置換する

- Actor: agent
- Depends on: P1-05
- Files: `src/app.rs`、`src/main.rs`、`src/metrics.rs`
- Actions:
  1. AppStateをconfig、target state、admission、HTTP client、metricsへ縮小する。
  2. `/healthz`、`/readyz`、`/cluster`、`/metrics`を追加する。
  3. Legacy `/backends`、`/affinity`を新request pathから切り離す。
  4. Public/admin listener addressを仕様値へ合わせる。
- Verification: Admin endpoint status/body test
- Done when: Solo Standalone targetだけでbinaryが起動可能

Evidence: `src/app.rs`、`src/main.rs`、`src/metrics.rs`、`src/proxy.rs`; schema v2 runtime configから単一target state/client/admissionを構築し、cluster無効時のSolo Standalone、仕様listener、mode-aware public path、`/healthz`・`/readyz`・`/cluster`・`/metrics`を接続。Legacy `/backends`・`/affinity`は新routerから除外。Admin status/body 2 tests、共通local gate（70 tests）、test-support gate（72 tests）、check/clippy成功; 2026-08-06

#### [x] P1-07 Legacy routing codeを除去する

- Actor: agent
- Depends on: P1-06
- Files: `src/routing.rs`、`src/backend.rs`、`src/affinity.rs`、`src/heartbeat.rs`、`src/persistence.rs`、`Cargo.toml`、`Cargo.lock`
- Actions:
  1. `rg`でproduction参照が0であることを確認する。
  2. Fileを削除する。
  3. `rusqlite`、`unicode-normalization`など直接利用がなくなったdependencyだけ削除する。
  4. Legacy SQLite fileをfilesystemから削除しない。
- Verification: 共通local gate、`cargo tree`で不要dependency不在
- Done when: Load balancing/affinity symbolがproduction sourceにない
- Stop when: Streaming helperがlegacy moduleに残る。先に移動してtestする

Evidence: `src/proxy.rs`へstreaming/header helperを保持したまま、`src/routing.rs`、`src/backend.rs`、`src/affinity.rs`、`src/heartbeat.rs`、`src/persistence.rs`と旧proxy path/schema/testsを削除。runtime型参照0件、旧SQLite実ファイルへの操作なし。`rusqlite`、`unicode-normalization`、`hmac`、`sha2`は`Cargo.toml`/lock/`cargo tree`から不在。共通local gate（40 tests）、test-support gate（42 tests）、check/clippy成功; 2026-08-06

#### [x] P1-08 Phase 1 integrationを固定する

- Actor: agent
- Depends on: P1-07
- Files: `ds4-smart-proxy.example.toml`、本書
- Actions: Exampleをparse可能なschema v2へ更新し、fake local upstreamでsmoke test
- Verification: 共通local gate、example parse、Solo streaming
- Done when: Phase 1 exit conditionとEvidenceを記録

Evidence: `ds4-smart-proxy.example.toml`、`tests/phase1_smoke.rs`; repository exampleをschema v2 parserで固定し、Fake DS4 → Solo Standalone fixed target → clientのunknown path/body/SSE逐次転送smoke成功。Phase 1 exit condition「Solo Standalone fixed targetでproxy test成功」を充足。共通local gate（41 tests）、test-support gate（44 tests）、check/clippy成功; 2026-08-06

### Phase 2: Thunderbolt discoveryとPaired Standalone

#### [x] P2-01 Cluster module skeletonとsingle-writer state machineを作る

- Actor: agent
- Depends on: P1-08
- Read: 仕様書第8、9、18、28章
- Files: `src/cluster/mod.rs`、`config.rs`、`state.rs`、`src/lib.rs`
- Actions: Event enum、command channel、single writer loop、read-only snapshotを実装
- Verification: Old generation、invalid transition、concurrent event unit test
- Done when: Child/networkをまだ操作せずstate transitionをtest可能

Evidence: `src/cluster/mod.rs`、`src/cluster/state.rs`; bounded command channel、single-writer loop、watch snapshot、generation照合、型付きevent/transition errorを実装。Old generation、invalid transition、16 concurrent same-generation eventsの直列化3 tests、共通local gate（44 tests）、test-support gate（47 tests）、check/clippy成功。Child/network操作なし; 2026-08-06

#### [x] P2-02 Network snapshotとrole判定を実装する

- Actor: agent
- Depends on: P2-01
- Read: 仕様書第13.1、13.2節
- Files: `src/cluster/network_snapshot.rs`、`role.rs`
- Actions: Service enabled、interface UP、IPv4/prefix、route scope、roleを型付きsnapshotへ変換
- Verification: Fixtureで全`ThunderboltIpState`、address conflict、unknown role
- Done when: Shell command parseなしでsnapshot生成

Evidence: `src/cluster/network_snapshot.rs`、`src/cluster/role.rs`; System Configuration/getifaddrs bindingから渡せる型付きobservationを8種の`ThunderboltIpState`、固定address role、expected peer/route/auth状態へ変換。全state、unknown/prefix/multiple-address conflict、wrong route fixture 4 tests、共通local gate（48 tests）、test-support gate（51 tests）、check/clippy成功。Shell command parseなし; 2026-08-06

#### [x] P2-03 Dynamic Store event monitorを実装する

- Actor: agent
- Depends on: P2-02
- Read: 仕様書第13.5節
- Files: `src/cluster/network_events.rs`、platform binding module
- Actions: Link/IPv4/Setup notification、500ms debounce、30s reconcile、generation ownershipを実装
- Verification: Duplicate/out-of-order fixture、cancel/drop test
- Done when: Eventはrescanだけを起こしmodeを直接変更しない

Evidence: `src/cluster/network_events.rs`、`src/cluster/platform/macos.rs`; bounded nonblocking callback channel、500ms設定可能debounce、30s設定可能reconcile、generation filter、last-handle cancellation、SCDynamicStore Link/IPv4/Interface/Setup subscriptionとRunLoop所有を実装。Event outputは型付きrescan requestのみ。duplicate/out-of-order/reconcile/drop/key分類4 tests、共通local gate（52 tests）、test-support gate（55 tests）、check/clippy成功。初回`cargo fetch`はsandbox DNS制限で失敗し、承認済みnetwork実行で`system-configuration 0.7.0`取得成功; 2026-08-06

#### [x] P2-04 Bonjour discoveryを実装する

- Actor: agent
- Depends on: P2-03
- Read: 仕様書第13.3節
- Files: `src/cluster/bonjour.rs`、`discovery.rs`
- Actions:
  1. `_ds4cluster._tcp`を`bridge0` indexだけへregister/browseする。
  2. Port network byte order、self result、subnet、routeを検証する。
  3. DNSServiceRef lifecycleをnetwork generationに結び付ける。
  4. Static fallbackを実装する。
- Verification: Self/wrong interface/wrong subnet/duplicate/permission failure test
- Done when: Candidate discoveryだけではpeer presentにならない

Evidence: `src/cluster/bonjour.rs`、`src/cluster/discovery.rs`、`src/cluster/platform/bonjour.rs`; `if_nametoindex("bridge0")`と同一interface index限定のDNS-SD register/browse/resolve/getaddrinfo、network byte order port、最小TXT（`protocol=1`、`node_id`）、IPv4限定address解決、`AsyncFd`駆動とgeneration連動lifecycleを実装。解決結果はself/interface/protocol/subnet/期待address/scoped routeを検証してcandidateだけを生成し、重複を除去。permission/policy/daemon failure時のみ同じroute検証を通したstatic fallbackを許可。self/wrong interface/wrong subnet/unexpected address/wrong route/duplicate/permission fallbackを含む5 tests、共通local gate（57 tests）、test-support gate（60 tests）、check/clippy成功。明示的な`-ldns_sd`はmacOS linker failureとなったため、C spikeと同じsystem export利用へ修正; 2026-08-06

#### [x] P2-05 HMAC control authenticationを実装する

- Actor: agent
- Depends on: P2-04
- Read: 仕様書第16章
- Files: `src/cluster/auth.rs`、`control.rs`
- Actions: Canonical signing、timestamp、nonce cache、body limit、constant-time verifyを実装
- Verification: Replay、clock skew、field mutation、wrong source test
- Done when: Secret/signatureがlogに出ない

Evidence: `src/cluster/auth.rs`、`src/cluster/control.rs`; METHOD/path+query/timestamp/nonce/lowercase SHA-256 body digestのcanonical HMAC-SHA256、constant-time `verify_slice`、32-byte以上secret、30秒clock skew、node ID/expected source IP検証、署名成功後のatomic nonce消費と5分TTL、64KiB逐次body accumulator、全control endpoint/header定義を実装。SecretはDrop時消去し、secret/signature/nonceを`Debug`でredact。既知signature vector、replay/TTL、clock skew、全signed field mutation、wrong source/node、body limit、redactionを含む8 tests、共通local gate（65 tests）、test-support gate（68 tests）、check/clippy成功。初回dependency取得はsandbox DNS制限で失敗し、承認済み`cargo fetch`で`hmac 0.12.1`/`sha2 0.10.9`取得成功; 2026-08-06

#### [x] P2-06 Peer leaseとcontrol endpointを実装する

- Actor: agent
- Depends on: P2-05
- Files: `src/cluster/control.rs`、`coordinator.rs`、`worker.rs`
- Actions: Pair、descriptor、lease renew/expiry、idempotency、generationを実装
- Verification: Lease中断、old generation、duplicate message test
- Done when: HMAC + route + stable leaseだけがpeer presentを作る

Evidence: `src/cluster/control.rs`、`src/cluster/coordinator.rs`、`src/cluster/worker.rs`; serde deny-unknown descriptor/message/response、全control commandとrole別許可、HMAC verifierだけが生成できる`AuthenticatedPeer` capability、protocol/node/role/scoped route検証、Pair、5秒descriptor poll renew対応の15秒lease、required stability、route invalidation/expiry、generation 409、deployment 412、request ID idempotency/conflictを実装。Peer presentは認証済みdescriptor + scoped route + stability経過 + 未失効leaseの積だけ。lease interruption/route loss、old generation、duplicate renew、changed duplicate、deployment mismatch、role方向を含む7 tests、共通local gate（72 tests）、test-support gate（75 tests）、check/clippy成功; 2026-08-06

#### [x] P2-07 Peer ingressを実装する

- Actor: agent
- Depends on: P2-06
- Read: 仕様書第10.2、10.3、13.4節
- Files: `src/proxy.rs`、`src/app.rs`、cluster coordinator module
- Actions: Coordinator-only bind、token、source IP、hop=1、shared admission/in-flightを実装
- Verification: Invalid token/source/hop、loop prevention、2-hop SSE test
- Done when: Worker requestがcoordinator local upstreamへ1経路だけで到達

Evidence: `src/proxy.rs`、`src/app.rs`; peer token raw bytesをlowercase hex bearerへ変換しconstant-time比較、expected worker source IP、hop=`1`を検証。Public入力のtoken/hop/cluster headerを除去し、Worker→Coordinator target時だけ内部token/hopを再生成。Peer ingressはCoordinator role + fixed coordinator address以外のbindを拒否し、認証後もLocalStandalone以外へ転送せず2-hop目を明示的に阻止。同じ`ModeAwareProxyState`のstreaming/body limit/admission/in-flightをpublic/peerで共有。invalid token/source/hop、untrusted header置換、wrong-role/wildcard bind、loop prevention、Worker public→Coordinator peer→local backendの2-hop SSEとpermit解放を含む6 tests、共通local gate（78 tests）、test-support gate（81 tests）、check/clippy成功; 2026-08-06

#### [ ] P2-08 Solo/Paired transitionを統合する

- Actor: agent
- Depends on: P2-07
- Files: `src/cluster/state.rs`、`coordinator.rs`、`worker.rs`、integration tests
- Actions: Required peer stability、worker drain、local placeholder stop hook、fallbackを実装
- Verification: Cable fixture、Bonjour loss only、lease expiry、peer reconnect test
- Done when: Fake upstreamでSolo → Paired → Soloが自動収束

### Phase 3: DS4 standalone supervisor

#### [ ] P3-01 DS4 command builderを実装する

- Actor: agent
- Depends on: P2-08
- Read: 仕様書第14、20、22章
- Files: `src/cluster/ds4_command.rs`、config tests
- Actions: Q2/Q2-Q4/MXFP4、resident/ssd-streaming、KV、context、typed SSD optionをargvへ生成
- Verification: Matrix snapshot test、forbidden duplicate arg test
- Done when: Shellを介さず完全argvを生成

#### [ ] P3-02 Process ownershipとidentity検証を実装する

- Actor: agent
- Depends on: P3-01
- Read: 仕様書第20.1–20.3節
- Files: `src/cluster/process.rs`、platform process module
- Actions: Process group、PID、executable、argv hash、start time、SIGTERM、optional verified SIGKILLを実装
- Verification: PID reuse/mismatch/unknown process test
- Done when: Unknown processへsignalするcode pathがない

#### [ ] P3-03 Child logとreadinessを実装する

- Actor: agent
- Depends on: P3-02
- Read: 仕様書第20.4、21章
- Files: `src/cluster/ds4_log.rs`、`process.rs`
- Actions: Non-blocking stdout/stderr、recognized event parser、HTTP readiness、startup timeoutを実装
- Verification: Known/unknown/truncated log、slow startup、early exit test
- Done when: Unknown logでstateが進まない

#### [ ] P3-04 Standalone lifecycleをstate machineへ接続する

- Actor: agent
- Depends on: P3-03
- Files: `src/cluster/state.rs`、`process.rs`、`src/app.rs`
- Actions: Boot start、ready target switch、Paired worker stop、peer loss restart、local drainを実装
- Verification: Fake child start/stop/crash/recovery integration
- Done when: Solo/Paired transitionが実child lifecycleを伴う

#### [ ] P3-05 Persistent cluster stateを実装する

- Actor: agent
- Depends on: P3-04
- Read: 仕様書第24章
- Files: `src/cluster/state_store.rs`
- Actions: Temp write、fsync、atomic rename、single-instance lock、corrupt preservationを実装
- Verification: Partial write、corrupt JSON、old generation、lock contention test
- Done when: Secretをstateへ保存しない

#### [ ] P3-06 Restart reconcileを実装する

- Actor: agent
- Depends on: P3-05
- Files: `src/cluster/state.rs`、`process.rs`、`state_store.rs`
- Actions: Owned child reattach/stop、port conflict manual state、desired/observed convergenceを実装
- Verification: Proxy restart with matching/mismatching child fixture
- Done when: Unknown port ownerをkillせずmanual stateへ入る

#### [ ] P3-07 Standalone actual acceptanceを実行する

- Actor: operator
- Depends on: P3-06
- Files: compatibility record、本書Evidence
- Actions: Q2 resident、Q2-Q4 SSD streaming、MXFP4 SSD streamingを対象Macで実行
- Verification: `/v1/models`、short prompt、streaming、memory/startup記録、24h supervisor run
- Done when: 各profileがpassまたは明示的blocked
- Stop when: 未確認profileをproduction readyと記録しない

### Phase 4: Distributed MXFP4 lifecycle

#### [ ] P4-01 Deployment manifestとfingerprintを実装する

- Actor: agent
- Depends on: P3-06
- Read: 仕様書第15章
- Files: `src/cluster/manifest.rs`、CLI job support
- Actions: Canonical JSON、async SHA-256、metadata cache/stale判定、deployment IDを実装
- Verification: Key order、file change、same/different digest test
- Done when: Handler threadで巨大modelを同期hashしない

#### [ ] P4-02 Generation付きdistributed controlを実装する

- Actor: agent
- Depends on: P4-01
- Read: 仕様書第16、17章
- Files: `control.rs`、`coordinator.rs`、`worker.rs`
- Actions: Prepare、ready、drain、demote、idempotency、old ack rejectionを実装
- Verification: Duplicate/reorder/drop control message test
- Done when: すべてのmutationがgenerationを検証

#### [ ] P4-03 DS4 HELLO parserを実装する

- Actor: agent
- Depends on: P4-02
- Read: 仕様書第17.2、17.3節
- Files: `src/cluster/ds4_hello.rs`、fixtures
- Actions: Network byte order、40-byte fixed payload、name length、deadline、trailing dataを検証
- Verification: Valid fixture、magic/type/size/truncate/fuzz test
- Done when: Arbitrary bytesでpanicしない

#### [ ] P4-04 Rendezvous listenerを実装する

- Actor: agent
- Depends on: P4-03
- Files: `src/cluster/ds4_hello.rs`、`coordinator.rs`
- Actions: Awaiting state限定bind、source/generation/deployment/layer確認、1 frame後closeを実装
- Verification: Wrong source/state/deployment/layer、timeout test
- Done when: AgentがHELLOを代理生成しない

#### [ ] P4-05 Worker distributed lifecycleを実装する

- Actor: agent
- Depends on: P4-04
- Files: `worker.rs`、`ds4_command.rs`、`process.rs`
- Actions: Drain、standalone stop、worker argv、HELLO、reconnect、lease、cleanupを実装
- Verification: Fake worker startup/timeout/exit/retry test
- Done when: Failure時にorphan workerが残らない

#### [ ] P4-06 Coordinator promotion/demotionを実装する

- Actor: agent
- Depends on: P4-05
- Files: `coordinator.rs`、`state.rs`、`admission.rs`
- Actions: Cluster-wide drain、standalone stop、coordinator start、worker registered、complete route、target switch、demotionを実装
- Verification: In-flight stream、route incomplete/loss、startup timeout test
- Done when: Complete route前にadmissionを再開しない

#### [ ] P4-07 Distributed integrationとactual acceptanceを実行する

- Actor: agent + operator
- Depends on: P3-07、P4-06
- Files: integration tests、compatibility record、本書Evidence
- Actions: Fake transition、実HELLO、short prompt、8K prefill、10回promotion/demotionを実行
- Verification: 仕様書第32.3、32.5、33.3節
- Done when: Fake test成功かつ実機結果をpass/blockedで記録

### Phase 5: Recovery、operations、security

#### [ ] P5-01 Failure policyとbackoffを完成する

- Actor: agent
- Depends on: P4-07
- Read: 仕様書第18.6、19、31章
- Files: `state.rs`、`coordinator.rs`、`worker.rs`
- Actions: Failure table全row、route grace、finite retry、manual stateを実装
- Verification: Table-driven failure integration test
- Done when: 同一failure 3回後にpromotion loop停止

#### [ ] P5-02 Admin APIとCLIを完成する

- Actor: agent
- Depends on: P5-01
- Read: 仕様書第23章
- Files: `src/main.rs`、`src/app.rs`、`src/cluster/admin.rs`または同等module
- Actions: Status、doctor、reconcile、pair、promote、demote、restart、fingerprintを実装
- Verification: Mutation auth、202 job、CLI does not spawn supervisor test
- Done when: CLI commandとadmin routeが仕様表に一致

#### [ ] P5-03 Loggingとmetricsを完成する

- Actor: agent
- Depends on: P5-02
- Read: 仕様書第25、26章
- Files: `src/metrics.rs`、logging call sites
- Actions: Spec metric/eventを実装し、high-cardinality/secret labelを禁止
- Verification: Metrics golden test、header/body redaction test
- Done when: Transition原因をpromptなしで診断可能

#### [ ] P5-04 macOS user serviceを作成する

- Actor: agent + operator for install
- Depends on: P5-03
- Read: 仕様書第35章
- Files: `contrib/launchd/ds4-smart-proxy.plist.example`、install guide draft
- Actions: RunAtLoad、KeepAlive、absolute args、finite throttle、single ownerを定義
- Verification: `plutil -lint`、login start、restart、no duplicate child
- Done when: DS4 childを別jobへ登録しない

#### [ ] P5-05 Dependencyとdead codeを整理する

- Actor: agent
- Depends on: P5-04
- Files: `Cargo.toml`、`Cargo.lock`、`src/`
- Actions: Direct usageを`rg`で確認し、unused dependency/module/featureだけ削除
- Verification: `cargo tree --duplicates`確認、共通local gate
- Done when: Legacy LB/affinity dependencyが残らない

#### [ ] P5-06 Securityとendurance gateを実行する

- Actor: agent + operator
- Depends on: P5-05
- Read: 仕様書第27、28、32、33.4節
- Files: tests、compatibility record、本書Evidence
- Actions: Secret permission、replay、wrong interface、unknown PID、cancellation storm、10 cable cycles、restartを検証
- Verification: 仕様書の該当acceptance checklist
- Done when: Safety/operations criteriaが全てpassまたは理由付きblocked

### Phase 6: 利用者向け文書

#### [ ] P6-01 配布用config exampleを確定する

- Actor: agent
- Depends on: P5-06
- Files: `ds4-smart-proxy.example.toml`、必要ならcoordinator/worker examples
- Actions: 実parserで成功する値だけを記載し、secret/model pathをplaceholder化
- Verification: Binary/doctorで全example parse
- Done when: Spec exampleとのfield差分を説明可能

#### [ ] P6-02 DS4を含む導入ガイドを作る

- Actor: agent + operator for command verification
- Depends on: P6-01
- Files: `docs/installation.md`
- Actions:
  1. Prerequisite、DS4 checkout/build、digest記録を書く。
  2. Model選択/取得/checksum/配置を書く。
  3. Resident/SSD streaming standalone smokeを書く。
  4. Thunderbolt固定IPv4、bridge/route確認を書く。
  5. Proxy build、secret、config、foreground testを書く。
  6. Pairing、promotion、LaunchAgent、recovery、upgrade、rollback、uninstallを書く。
- Verification: Clean user accountと両nodeでcommandを順番に実行
- Done when: 既存DS4環境なしからDistributedReadyへ到達
- Stop when: Model配布条件や未確認URLを推測しない

#### [ ] P6-03 READMEを全面刷新する

- Actor: agent
- Depends on: P6-02
- Files: `README.md`
- Actions:
  1. Mode-aware reverse proxy / supervisorとして説明する。
  2. 3 mode topology、profile matrix、quick start、security、limitationsを書く。
  3. Installation、spec、compatibility、operationsへlinkする。
  4. Least-busy、affinity、SQLite、EWMA、alternate retry説明を全削除する。
- Verification: `rg`でlegacy term不在、link check、command smoke
- Done when: READMEが実装済みbehaviorだけを説明

#### [ ] P6-04 Operationsとtroubleshootingを作る

- Actor: agent
- Depends on: P6-03
- Files: `docs/operations.md`、`docs/troubleshooting.md`
- Actions: Status、doctor、logs、metrics、manual state、safe restart、rollbackをfailure symptom別に記載
- Verification: Failure fixtureごとに該当手順がある
- Done when: Destructive cache削除を通常手順にしない

#### [ ] P6-05 文書をclean環境で検証する

- Actor: operator
- Depends on: P6-04
- Files: documentation evidence
- Actions: README/installationを先頭から実行し、copy/paste、期待出力、link、両roleを確認
- Verification: Clean 2-node installation
- Done when: Phase 6 exit conditionをevidence付きで満たす

### Phase 7: Migrationとrelease

#### [ ] P7-01 CIとbranch protectionをrelease条件へ合わせる

- Actor: agent + operator
- Depends on: P6-05
- Files: `.github/workflows/`、repository settings
- Actions: fmt、clippy、unit/integration testをrequired checkにする
- Verification: PR上で各check成功、failure時merge不可
- Done when: `CONTRIBUTING.md`のprotectionを満たす

#### [ ] P7-02 Legacy migrationとrollback rehearsalを行う

- Actor: operator
- Depends on: P7-01
- Files: migration evidence、release notes draft
- Actions: Legacy config rejection、旧SQLite非変更、新旧config分離、binary rollback、standalone readinessを確認
- Verification: Upgrade後にrollbackし、再度upgrade
- Done when: User data削除なしで往復成功

#### [ ] P7-03 Final acceptanceとrelease artifactを作る

- Actor: agent + operator
- Depends on: P7-02
- Files: compatibility record、release notes、artifacts
- Actions: 仕様書第33章を全確認し、binary checksum、DS4 baseline、known limitationsを記録
- Verification: 共通local gate、actual acceptance、document gate
- Done when: Blocked項目が0。例外は仕様変更として承認が必要

#### [ ] P7-04 Mainへ統合して計画を閉じる

- Actor: operator
- Depends on: P7-03
- Files: Git refs、本書、release record
- Actions:
  1. `rewrite/mode-aware`をphase履歴を保持して`main`へmergeする。
  2. `main`で全gateを再実行する。
  3. Release tagを作成する。
  4. `legacy/load-balancer-v1`と`load-balancer-v1-final`を保持する。
  5. 本書のevidenceをrelease recordへ移し、本書をarchiveする。
- Verification: Main CI、tag target、rollback ref
- Done when: Mainが新仕様を提供し、legacyとrollbackが到達可能
- Stop when: Integration branch全体を1 commitへsquashする必要がある

## 8. Phase gate一覧

| Gate | 必須task | Exit condition |
|---|---|---|
| Transition | T-01–T-03 | Legacy refとrewrite branchが安全に分離 |
| Phase 0 | P0-01–P0-06 | Baseline、fixture、API選定、fake DS4が揃う |
| Phase 1 | P1-01–P1-08 | Solo Standalone fixed targetでproxy test成功 |
| Phase 2 | P2-01–P2-08 | Cable eventからPaired/Soloへ自動収束 |
| Phase 3 | P3-01–P3-07 | Standalone profileとchild recoveryが実機成功 |
| Phase 4 | P4-01–P4-07 | Distributed promotion/demotionがfake/実機成功 |
| Phase 5 | P5-01–P5-06 | Recovery、admin、metrics、service、安全性が完成 |
| Phase 6 | P6-01–P6-05 | Clean 2-node導入が文書だけで成功 |
| Phase 7 | P7-01–P7-04 | Main release、legacy保存、rollback可能 |

T-04はrepository host設定の都合でP7-01まで延期可能だが、P7-03より前に完了する。

## 9. Task実行時の変更報告template

```text
Task: P?-?? <name>
Status: Complete | Blocked
Changed:
- <file>: <result>
Verification:
- <command>: PASS | FAIL
Evidence:
- <commit/artifact>
Remaining risk:
- <none or concrete risk>
Next eligible task:
- <task id>
```

「おそらく成功」「実装したはず」などの曖昧な完了報告を使用しない。実行していないhardware testは`未実行`と書く。
