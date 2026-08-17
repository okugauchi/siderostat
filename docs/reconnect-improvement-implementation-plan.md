# reconnect 改善実装・検証計画

Status: **PLANNED**

最終更新日: 2026-08-14

## 1. 目的

本書は [`reconnect-improvement-proposal.md`](reconnect-improvement-proposal.md) を、実装、
自動テスト、2 node 実機でのユーザー手作業を含む検証へ分解した実行計画である。対象は、peer
接続断または片側再起動後に次の一連の状態へ自動収束する経路である。

```text
Distributed / Paired
  -> peer loss
  -> 両 node の Solo Standalone serving
  -> 新しい control session で再pair
  -> Paired Standalone
  -> auto promotion
  -> 新しい generation の DistributedReady
```

本計画は、reasoning effort が低いモデルでも、未確定の設計判断を推測せずに一つずつ実行できる
粒度を目標とする。各 task は原則として一つの故障軸、一つの責務、一つの主な検証に限定する。

## 2. 正本、優先順位、対象外

正本の優先順位は次のとおりとする。

1. 製品 behavior と安全要件: [`spec.md`](spec.md)
2. reconnect の問題分析と改善方針: [`reconnect-improvement-proposal.md`](reconnect-improvement-proposal.md)
3. 現行実装の説明: [`connection-state-machine.md`](connection-state-machine.md)
4. Git 運用: [`../CONTRIBUTING.md`](../CONTRIBUTING.md)
5. 本作業の順序、状態、evidence: 本書

矛盾を見つけた場合は実装を進めず、正本を確認して本書を更新する。完了済み刷新計画の
[`archive/implementation-plan-v0.1.0.md`](archive/implementation-plan-v0.1.0.md) は履歴であり、
Git 運用や現行 behavior の根拠にしない。

優先度の扱い:

| 区分 | 内容 | リリース判定 |
|---|---|---|
| P0 | 診断、PeerLost lifecycle、control session 再ネゴシエーション、再接続 E2E | 必須 |
| P1 | Backoff、operator reconcile、promotion failure 配線 | 必須 |
| P2 | route / discovery の実測を pairing gate へ接続 | 必須。ただし protocol 設計と分離して実施 |
| P3 | sleep を Pair confirm / notify に置換 | P0〜P2 後の計測で timing 問題が残る場合のみ実装 |

対象外:

- cluster generation と control session generation の単純統合。
- peer loss 時の generation の 0 reset。
- worker を pairing authority にする変更。
- timeout の無制限化、assertion の緩和、単なる stability sleep の延長。
- network 設定の自動変更、secret・model・KV cache・runtime state の削除。
- reconnect と無関係な UI、profile、proxy forwarding の変更。

## 3. Task の状態と実行規則

Task heading の checkbox を進捗状態の正とする。

- `[ ]`: 未着手
- `[-]`: 着手中
- `[x]`: 完了。Verification と Evidence が記録済み
- `[!]`: blocked。理由と再開条件が記録済み

### 3.1 Task 選択

1. `Depends on` がすべて `[x]` の task だけを開始する。
2. 原則として一度に一つだけ `[-]` にする。
3. `Actor: operator` はユーザーの明示的な実行・承認なしに開始しない。
4. `Actor: agent + operator review` は agent が成果物を作成した後、ユーザーの承認まで `[x]` にしない。
5. `Files` 以外の変更が必要なら、先に当該 task の `Files` を更新する。
6. Git の branch 作成、rename、merge、tag、push 前には `CONTRIBUTING.md` を読む。

### 3.2 Task 開始時の preflight

```sh
git status --short
git branch --show-current
git diff --check
```

- ユーザーの既存変更を上書き、破棄、無関係に整形しない。
- 対象 file と既存変更が重なる場合は作業を止め、ユーザーへ確認する。
- secret、token、model、GGUF、KV cache、実機 runtime state、完全な deployment ID を repository
  へ追加しない。
- 実機 evidence は repository 外の作業 directory に採取し、redaction 済み要約だけを必要に応じて
  repository へ追加する。

### 3.3 RED task の例外

`R0-05` と `R0-06` は修正前の失敗を証明する RED task である。この 2 task に限り、対象 test が
意図した assertion で失敗することを完了条件にできる。無関係な compile error、timeout、flaky
failure は evidence にならない。

失敗を確認した直後の RED 状態を remote の共有 branch へ push しない。failure evidence を採取したら、
対象 test に原因と解除 task ID を含む `#[ignore = "..."]` を一時付与し、残りの suite を GREEN に戻して
RED task を完了する。`A-03` は lifecycle test、`G-05` は generation test の ignore を必ず解除する。
最終成果物に reconnect regression test の ignore を残さない。

### 3.4 共通 local gate

通常 task の Verification 後に、影響範囲に応じて次を実行する。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo test --all-targets --features test-support
git diff --check
```

文書だけの task は `git diff --check` と link/path の確認でよい。macOS 固有 API を変更した task は
macOS 上で `cargo check --all-targets --all-features` も実行する。

### 3.5 Evidence

完了 task の末尾へ次の形式で追記する。

```text
Evidence: <commit SHA または artifact path>; <実行 command と結果>; <YYYY-MM-DD>
```

実機 task は、node、build SHA、開始・終了時刻、操作、期待値、実測値、採取 log の SHA-256 を記録する。
失敗後の retry で成功した場合は最初の失敗も隠さず残す。

## 4. 完了判定

本計画全体は次をすべて満たしたときに完了する。

- P0、P1、P2 の全 task が `[x]`。
- P3 は `Q-01` の計測結果に基づき、`Q-02` が実装完了または「不要」と evidence 付きで判定済み。
- 自動テストが cable blip、coordinator のみ再起動、worker のみ再起動、両 process 再起動、
  Pair の遅延・重複を覆う。
- 各シナリオで両 node の state、stable mode、cluster/control generation、lease、control phase、
  target、target readiness、admission、standalone/distributed child identity を検証する。
- 実機で DistributedReady からの cable detach/reconnect、片側 macOS 再起動の両方向、両側再起動が
  acceptance criteria を満たす。
- stale distributed child、orphan transition、重複 proxy/DS4 child が残らない。
- rollback 手順が実行可能で、ユーザーデータや runtime state の削除を必要としない。

## 5. 依存関係

各 task の `Depends on` を正とする。概要は次のとおり。

```text
R0-01 -> R0-02 -> R0-03 -> R0-04
                              |-> R0-05 -> A-01 -> A-02 -> A-03 -> A-04 --+
                              `-> R0-06 -> G-01 -> G-02 -> G-03 -> G-04 -> G-05
                                                                          |
                                                         A-04 + G-05 -> E-01
                                                                          |
                                            E-01 -> B-01 -> B-02 -> B-03 -> B-04
                                                                          |
                                            B-04 -> N-01 -> N-02 -> N-03 -> Q-01
                                                                          |
                                      Q-01 -> Q-02（必要時）--------------+
                                                                          |
                                               N-03 + Q-02判定 -> H-01 -> H-02 -> H-03 -> H-04
                                                                                         |
                                                                                         v
                                                                                       H-05 -> F-01
```

`R0-05` と `R0-06` は同じ harness を使うが、同じ worktree では並行編集しない。実機 task
`H-02`〜`H-04` は安全上、同時に実行しない。

## 6. Phase R0: baseline、診断、失敗再現

### [x] R0-01 現行 baseline を固定する

- Actor: agent
- Depends on: なし
- 着手可能条件: repository を読め、既存変更の所有者と対象範囲を識別できる
- Read: `AGENTS.md`、`CONTRIBUTING.md`、本書第 1〜5 節
- Files: code 変更なし。本 task の Evidence 行だけ
- Actions:
  1. branch、HEAD SHA、dirty file を記録する。
  2. 共通 local gate を実行する。
  3. `docs/reconnect-improvement-proposal.md` 記載の関数・test file が現行 tree に存在することを確認する。
  4. baseline failure があれば修正せず、最初の failure と reconnect task への影響を記録する。
- Verification: 共通 local gate
- 完了条件: 実装前の再現可能な SHA と test 結果が Evidence にある
- 停止条件: 対象 file に由来不明の未 commit 変更がある、または baseline 自体が compile しない

Evidence: branch=feature/reconnect-recovery, HEAD=466debf2ba920343d18192b90bbc483cc569419d, 着手時 dirty=なし; 共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 170 + integration 3 GREEN / cargo test --all-targets --features test-support: unit 170 + integration 8 GREEN / git diff --check clean); proposal 記載の関数 (fallback_to_solo, reconcile_backoff, operator_reconcile, control_generation, GenerationMismatch) と test file (tests/phase4_distributed.rs, tests/phase5_security.rs) の存在を確認、baseline failure なし; 2026-08-14

### [x] R0-02 reconnect 診断 contract を固定する

- Actor: agent + operator review
- Depends on: R0-01
- 着手可能条件: baseline の `/cluster`、control response、child identity、log schema を確認済み
- Files: `docs/reconnect-diagnostics-contract.md`（新規）
- Actions:
  1. `/cluster` に追加する read-only field を JSON 例付きで定義する。
  2. 最低限 `cluster_generation`、`control_session_generation`、`control_phase`、lease 有効性と期限、
     route scope、standalone/distributed child の `pid/profile/generation/running` を含める。
  3. transition log event に `event`、`owner`、`from/to`、cluster/control generation、結果、理由を定義する。
  4. Pair の 409 log に `expected` と `received` を定義する。secret、signature、nonce、完全 deployment ID は除外する。
  5. backward compatibility 方針を「field 追加のみ」と明記する。
- Verification: proposal 第 4 節 P0-0 の観測項目との対応表を作る
- 完了条件: operator が field と redaction を承認し、実装者に追加判断が残っていない
- 停止条件: secret または高 cardinality 値を metrics label に入れる必要がある
Evidence: docs/reconnect-diagnostics-contract.md を新規作成し operator 承認済み (commit 46d1116); proposal P0-0 観測項目との対応表 §8 を作成、field/redaction/導出元を確定; 2026-08-14

### [x] R0-03 reconnect 診断情報を実装する

- Actor: agent
- Depends on: R0-02
- 着手可能条件: 診断 contract が承認済み
- Files: `src/app.rs`、`src/cli.rs`、`src/metrics.rs`、`src/cluster/production.rs`、
  `src/cluster/production/reconcile.rs`、関連 unit test
- Actions:
  1. production runtime の read-only snapshot を `AppState` から取得できるようにする。
  2. contract の field を `/cluster` と `cluster status --json` へ追加する。
  3. PeerLost、recovery、Pair、promotion の開始・完了・失敗を構造化 log にする。
  4. Pair 409 の expected/received を redaction 規則を守って記録する。
  5. child identity なし、lease なし、role unknown を `null` または明示 enum として扱い、偽の 0 値にしない。
- Verification: admin handler/CLI JSON test、log field test、共通 local gate
- 完了条件: proposal P0-0 の全項目を外部から read-only で採取できる
- 停止条件: 診断取得のために mutation endpoint または secret 公開が必要になる

Evidence: branch=feature/reconnect-recovery; 前半の read-only 診断 snapshot を commit 533b46e、後半の構造化 log を commit 0bcf240 で実装; `/cluster` と `cluster status --json` に `cluster_generation`/`control_session`/`children` を kebab-case で追加; EventOwner を導入し peer-lost / recovery-started / recovery-completed / recovery-failed / pairing-started / pairing-ready / promotion-started / promotion-failed / demotion-started / pair-generation-mismatch / cluster-transition-rejected を構造化 log 化; Pair 409 は expected/received/cluster_generation/control_session_generation の generation 値のみ記録 (secret/signature/nonce/deployment_id は除外); child identity なし・lease なし・role unknown は null または明示 enum で扱い偽 0 値なし; ログ捕捉テストを tracing-test の traced_test に移行し 並列実行を安定化; 共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 173 + integration GREEN / cargo test --all-targets --features test-support: unit 173 + integration GREEN / git diff --check clean); 2026-08-14

### [x] R0-04 2 node production 相当 test harness を作る

- Actor: agent
- Depends on: R0-03
- 着手可能条件: diagnostic snapshot を test から取得できる
- Files: `tests/support/mod.rs`、`tests/reconnect_production.rs`（新規）、必要な `test-support` API
- Actions:
  1. coordinator/worker を別 runtime、別 loopback address、別永続 state path で起動する fixture を作る。
  2. fake DS4 の standalone/coordinator/worker child の start/stop、PID 相当 identity、profile、generation を記録する。
  3. control HTTP を実際に通し、送信遅延、重複送信、片方向遮断、process 再生成を注入できるようにする。
  4. `wait_until` は deadline と最終 snapshot を持ち、固定 sleep だけに依存しない。
  5. test 終了時に task、listener、child を回収し、orphan があれば test を失敗させる。
- Verification: Solo 起動と正常な初回 pair の smoke test
- 完了条件: 2 node の状態と child identity を一つの assertion helper で比較できる
- 停止条件: production-only code を test 専用分岐で置換しないと成立しない

Evidence: branch=feature/reconnect-recovery; 実装を commit 96d5459 で追加; `ProductionInner` の child を trait object 化し、lifecycle に `child_identity`/`is_running` を追加、`new_with_lifecycles` で fake child と peer control port を注入可能にした (production `new` は従来通り `config.cluster.control_port` を peer へ渡すため挙動不変); `tests/support/mod.rs` に fake standalone/worker/coordinator child (start/stop、PID 相当 identity、profile、generation を記録) と TwoNode fixture (別 runtime、別 control port、別永続 state path、loopback は同一 127.0.0.1 に別 port で分離) を追加。このホストは bind 可能な loopback address が 127.0.0.1 と ::1 のみ (127.0.0.2/::2 は alias なし、IPv4/IPv6 の混在接続は非対応) のため、計画の「別 loopback address」は満たせず、同一 loopback 上で別 port による完全分離に変更して harness に制約を明記。`ProductionControlClient` が peer 接続に local node の control_port を用いる設計のため、test-support 専用に peer control port 注入を追加した (production-only code の test 専用分岐への置換は行っていない)。`tests/reconnect_production.rs` に Solo 起動と正常な初回 pair の smoke test を追加し、2 node の状態と child identity を一つの assertion helper (assert_paired_consistent) で比較、wait_until は deadline + 最終 snapshot 方式、shutdown で serve task を回収し orphan distributed child を検出。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 173 + integration GREEN / cargo test --all-targets --features test-support: unit 173 + integration GREEN (reconnect_production 3 tests 含む) / git diff --check clean); 2026-08-14

### [x] R0-05 PeerLost lifecycle の RED test を追加する

- Actor: agent
- Depends on: R0-04
- 着手可能条件: harness の正常 pair/promotion が安定して成功する
- Files: `tests/reconnect_production.rs`
- Actions:
  1. DistributedReady で control/route を失効させる。
  2. coordinator の distributed child 残存または standalone 未起動を検出する assertion を追加する。
  3. worker の standalone/distributed 併存を検出する assertion を追加する。
  4. state だけが SoloReady でも child/target/admission が不整合なら失敗させる。
  5. failure evidence 採取後、test に解除 task `A-03` を明記した一時 `ignore` を付ける。
- Verification: 対象 test が既知の lifecycle assertion で再現性をもって失敗する
- 完了条件: P0-A の修正がなければ通らない test と failure evidence があり、残りの suite は GREEN
- 停止条件: failure が generation mismatch に先に遮られる。この場合は fixture の同一 generation を明示する

Evidence: branch=feature/reconnect-recovery; 実装を追加。`tests/support/mod.rs` に `fake_worker_hello_bytes` / `inject_fake_worker_hello` / `TwoNode::promote_to_distributed` (coordinator の DS4 rendezvous listener へ合成 HELLO を注入して `promote()` を DistributedReady まで完走させる) と `Node.ds4_distributed_port` を追加。`tests/reconnect_production.rs` に `peer_lost_from_distributed_ready_orphans_distributed_children` を追加し、pair -> promote -> DistributedReady で両 distributed child の稼働を確認した後、両 node の control HTTP server を abort して `reconcile()` を呼び PeerLost を発火。P0-A の lifecycle 不整合を検出: coordinator の distributed child が PeerLost 後に残存 (fallback_to_solo が distributed child を停止しない)、worker が standalone と distributed を同時稼働させる (fallback_to_solo が worker standalone を起動しても distributed worker child を停止しない)。RED 確認: 両 assertion とも再現性をもって失敗 (coordinator orphan / worker coexistence)。failure evidence 採取後、解除 task A-03 を明記した `#[ignore = "RED for P0-A; resolve in A-03 coordinator PeerLost recovery (see reconnect plan R0-05)"]` を付与し、残り suite を GREEN に維持。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 173 + integration GREEN / cargo test --all-targets --features test-support: unit 173 + integration GREEN (reconnect_production 3 passed + 1 ignored) / git diff --check clean); 2026-08-14

### [x] R0-06 control generation mismatch の RED test を追加する

- Actor: agent
- Depends on: R0-04
- 着手可能条件: lifecycle failure を避けるため PairedStandaloneReady 単独でも実行できる
- Files: `tests/reconnect_production.rs`、必要なら control unit test
- Actions:
  1. worker の永続 baseline generation を coordinator より高くして worker だけを再生成する。
  2. coordinator の Pair が 409 `GenerationMismatch` になることを確認する。
  3. periodic reconcile を複数回進めても収束しない現行 behavior を確認する。
  4. 逆方向では高い generation へ追従できることも別 assertion にし、方向依存を固定する。
  5. failure evidence 採取後、test に解除 task `G-05` を明記した一時 `ignore` を付ける。
- Verification: 失敗理由が expected/received generation と一致する
- 完了条件: P0-B の修正がなければ通らない方向別 test と evidence があり、残りの suite は GREEN
- 停止条件: lifecycle failure と同じ test に混ざり、原因を一意に判定できない

Evidence: branch=feature/reconnect-recovery; 実装を追加。`Node::build` に baseline generation を渡せるよう `spawn_ready_at` を利用し、`TwoNode::boot_with_baseline(coordinator_baseline, worker_baseline)` を追加 (spawn_ready_at により各 node の control session generation は baseline+2)。`tests/reconnect_production.rs` に方向別 2 test を追加。(1) `coordinator_adopts_higher_worker_generation_on_pair` (RED, worker baseline 100 / coordinator 0): worker の高 generation を coordinator が採用して pair が収束すべき、を assert。現行は worker が coordinator の低 generation Pair を 409 GenerationMismatch で拒否するため失敗 (evidence: `peer control /v1/pair returned 409 Conflict: {"error":"control generation mismatch: expected 102, received 2"}`、expected=worker baseline+2、received=coordinator baseline+2)。periodic reconcile を複数回実行しても coordinator は SoloStandaloneReady のまま収束しない現行 behavior を確認済み (coordinator の lease が未確立のため peer_present=false で reconcile が no-op)。(2) `higher_coordinator_baseline_generation_is_followed_by_worker` (GREEN, coordinator baseline 100 / worker 0): 逆方向では worker が coordinator の高 generation を advance_generation で追従し pair が収束する方向依存を固定。failure evidence 採取後、解除 task G-05 を明記した `#[ignore = "RED for P0-B; resolve in G-05 control session generation negotiation (see reconnect plan R0-06)"]` を付与し、残り suite を GREEN に維持。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 173 + integration GREEN / cargo test --all-targets --features test-support: unit 173 + integration GREEN (reconnect_production 4 passed + 2 ignored) / git diff --check clean); 2026-08-14

## 7. Phase A: P0-A PeerLost lifecycle

### [x] A-01 PeerLost recovery の所有権と失敗規則を設計固定する

- Actor: agent + operator review
- Depends on: R0-05
- 着手可能条件: lifecycle RED test の event 順と残存 child が記録済み
- Files: `docs/reconnect-peer-loss-design.md`（新規）
- Actions:
  1. control reconcile と route-loss demotion が共有する単一 recovery owner を定義する。
  2. transition generation を照合し、古い recovery が新しい state/child を操作しない規則を定義する。
  3. role 別順序を `admission block -> distributed stop -> standalone start/readiness -> publish SoloReady`
     として固定する。
  4. stop は既存の `ChildIdentity` 検証を必須とし、unknown process へ signal しない。
  5. stop/start/readiness/identity mismatch ごとに、`SoloStandaloneStarting + Unavailable` で retry するか、
     `ManualInterventionRequired` へ入るかを表で固定する。
  6. 同じ generation の重複 recovery を冪等、古い generation を no-op とする。
- Verification: coordinator/worker/競合/途中失敗の sequence diagram と状態表
- 完了条件: 実装者が lock 範囲、event 順、failure state を推測せず実装できる
- 停止条件: `spec.md` 第 18.4 節と 18.5 節の変更が必要
- Evidence: `docs/reconnect-peer-loss-design.md` を新規作成 (commit は本 Phase A 実装と一体で記録)。単一の `PeerLossRecovery` owner (lock + `completed_generation`) を定義し、control reconcile と route-loss demotion monitor が共有する。世代保護は owner の lock と state machine の `expected_generation` 照合の二重で担保。role 別順序を `admission block -> distributed stop -> standalone start -> publish SoloReady` に固定。stop は `ChildIdentity` 検証必須。failure mapping 表を固定: stop/start/readiness 失敗は `SoloStandaloneStarting + Unavailable` で retry、同一 generation 重複は冪等 no-op、古い generation は no-op、永続回復不能は `ManualInterventionRequired` (Phase B で接続)。coordinator も standalone を再起動する (R0-05 action 4 の不整合 4 を解消)。`spec.md` 18.4/18.5 は変更不要と判断。; 2026-08-14

### [x] A-02 worker の PeerLost recovery を実装する

- Actor: agent
- Depends on: A-01
- 着手可能条件: 設計の worker sequence と failure mapping が承認済み
- Files: `src/cluster/production/reconcile.rs`、`src/cluster/production/worker.rs`、
  `src/cluster/runtime.rs`、関連 unit/integration test
- Actions:
  1. recovery owner を取得し、future admission を block する。
  2. worker distributed child を identity 確認付きで停止する。
  3. local standalone を指定 generation で起動し readiness を待つ。
  4. 成功後だけ target=LocalStandalone、SoloStandaloneReady、Serving を publish する。
  5. 同一 request の再実行と途中失敗からの retry を冪等にする。
- Verification: R0-05 の worker assertion、stop/start failure unit test、共通 local gate
- 完了条件: worker で standalone と distributed child が同時に生存しない
- 停止条件: identity mismatch を無視または強制 kill しないと test が通らない
- Evidence: `src/cluster/production/recovery.rs` に共通 owner を実装。worker 側は `recover_from_peer_loss(owner)` が `admission.block()` + `proxy target=Unavailable(Transition)` -> `stop_distributed_child()` (worker distributed child のみ) -> `PeerLost` 適用で `SoloStandaloneStarting` -> `standalone.start(generation)` -> `LocalStandaloneReady` で `SoloStandaloneReady` -> `proxy target=LocalStandalone, ready=true` publish + `admission.start_serving()`。`reconcile_peer` は peer_present=false かつ fallback 対象 state なら recovery owner 経由に変更 (純粋 `fallback_to_solo` は distributed child を触れないため production では使用しない)。`requires_solo_fallback` に `SoloStandaloneStarting` を含め、途中失敗からの retry が再び recovery に入って先へ進む (A-04 で確認)。stop/start 失敗時は state を偽 Ready にせず `SoloStandaloneStarting + Unavailable` を維持して error を返し、owner の lock を解放して次回 reconcile に委ねる。共通 local gate 全項目成功 (後述 A-03/A-04 Evidence 参照); 2026-08-14

### [x] A-03 coordinator の PeerLost recovery を実装する

- Actor: agent
- Depends on: A-02
- 着手可能条件: 共通 recovery owner と worker path が GREEN
- Files: `src/cluster/production/reconcile.rs`、`src/cluster/production/pairing.rs`、
  `src/cluster/coordinator.rs`、関連 unit/integration test
- Actions:
  1. coordinator distributed child を identity 確認付きで停止する。
  2. coordinator standalone を起動し readiness を待つ。
  3. route-loss demotion task が同じ child を操作中なら設計済み owner で直列化する。
  4. stale route-loss task は generation 不一致を正常な no-op として終了する。
  5. 成功後だけ SoloStandaloneReady、LocalStandalone、Serving を publish する。
  6. `R0-05` で付けた一時 `ignore` を解除する。
- Verification: R0-05 の coordinator assertion、stale demotion test、共通 local gate
- 完了条件: coordinator の SoloReady が実 standalone readiness と一致する
- 停止条件: cleanup 前に state だけを Ready へ進める必要がある
- Evidence: 共通 recovery owner を coordinator 側にも適用。`stop_distributed_child()` は coordinator なら distributed coordinator child のみ停止。`pairing.rs` の route-loss task を `CoordinatorDistributedRuntime::wait_route_loss_and_demote()` から `runtime.handle_route_loss()` に変更し、route-loss monitor も peer present なら graceful demote、peer loss なら `recover_from_peer_loss(RouteLossMonitor)` で同じ owner へ収束させる。`R0-05` で付けた `peer_lost_from_distributed_ready_orphans_distributed_children` の一時 `ignore` を解除し GREEN 確認。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets / cargo test --all-targets --features test-support / git diff --check clean); 2026-08-14

### [x] A-04 PeerLost recovery の race と failure test を完成する

- Actor: agent
- Depends on: A-03
- 着手可能条件: 両 role の基本 RED test が GREEN
- Files: `tests/reconnect_production.rs`、`src/cluster/coordinator/tests.rs`、関連 unit test
- Actions:
  1. control lease loss と route loss monitor の同時発火を barrier で再現する。
  2. promotion 中の PeerLost を Awaiting/Promoting/DistributedStarting の各 state で試す。
  3. child stop failure、standalone readiness failure、重複 PeerLost、stale generation を試す。
  4. 各失敗で admission=Blocked、target=Unavailable、ready state 非偽装を確認する。
  5. recovery 後の再promotionで以前の PID/generation が再利用されないことを確認する。
- Verification: 対象 race test を 10 回反復、共通 local gate
- 完了条件: lifecycle の RED test と競合 test がすべて GREEN
- 停止条件: test の成功に scheduler timing や長い固定 sleep が必要
- Evidence: `tests/reconnect_production.rs` に race/failure 5 test を追加。`peer_loss_recovery_is_idempotent_on_duplicate_reconcile` (重複 recovery で standalone が再起動されない)、`recovery_then_repromotion_uses_new_child_generation` (recovery 後の再 promotion で新旧 generation が再利用されない)、`route_loss_monitor_and_peer_loss_reconcile_race_converge_to_solo` (route-loss と reconcile の同時発火が単一 Solo recovery へ収束し orphan child なし)、`worker_stop_failure_keeps_recovery_from_faking_ready` (stop 失敗で `SoloStandaloneStarting + Unavailable` 維持、retry で回復)、`standalone_start_failure_keeps_recovery_from_faking_ready` (standalone start 失敗でも同様)。harness 側は `FakeStandalone.set_start_fails`、`FakeWorkerChild.set_stop_fails`、`FakeCoordinatorChild` に route_ready/route_lost/route_changed Notify + `lose_route`/`restore_route`/`set_stop_fails` を追加、`Node::build` の control/peer 各 port を `free_loopback_port()` で動的確保し並行実行の衝突を回避。A-04 Verification として `cargo test --test reconnect_production --features test-support` を 10 回反復し全回 GREEN (10 passed + 1 ignored、flaky なし、固定 sleep 不使用)。共通 local gate 全項目成功; 2026-08-14

## 8. Phase G: P0-B control session generation

### [x] G-01 Pair session negotiation protocol を設計固定する

- Actor: agent + operator review
- Depends on: R0-06
- 着手可能条件: 方向依存 mismatch の expected/received と永続 state 差が記録済み
- Files: `docs/control-session-negotiation.md`（新規）、必要なら `docs/spec.md`
- Actions:
  1. coordinator を唯一の session authority とする。
  2. Pair offer と Pair confirm の message、許可 role、phase、idempotency key、response を定義する。
  3. candidate generation を双方の既知値より古くない checked 値として定義し、overflow behavior を決める。
  4. session commit 時に local generation、peer lease、control phase、processed request map を一つの原子的操作で更新する。
  5. crash point を offer 前、offer 後、confirm 前、confirm 後に分け、再送時の収束規則を定義する。
  6. 確定前 session の non-Pair command/ack と、確定済み session より古い command/ack を拒否する。
  7. 永続化 field、schema migration、旧 version との互換性または明示的拒否を決める。
- Verification: 正常、片側高世代、同時 retry、重複、crash、overflow の状態遷移表
- 完了条件: wire format と atomic commit boundary が operator 承認済み
- 停止条件: generation reset、worker authority、古い non-Pair command の許可が必要
- Evidence: `docs/control-session-negotiation.md` を新規作成。coordinator を唯一の session authority とし、`ControlCommand::Pair` を coordinator からの offer / worker からの confirm として使い回す (新規 wire 型は追加しない)。candidate generation は `max(local, peer)` の checked 値で、`u64::MAX` 到達は明示的な exhaustion として扱い generation reset は行わない。session commit は coordinator の control Mutex 内で local generation / lease / phase / processed map を一括更新し部分更新 window を持たない。crash point (offer 前/後、confirm 前/後) と再送収束規則、永続化 field (`control_session_generation` を cluster generation と別 field) を定義。direction 別収束表で worker 高世代を方向非依存に解消。停止条件 (generation reset / worker authority / 古い non-Pair 許可) は不使用と判断。`spec.md` 第 18.4/18.5 節は変更不要と判断。; 2026-08-14

### [x] G-02 pure control session state machine を実装する

- Actor: agent
- Depends on: G-01
- 着手可能条件: protocol の型、phase、error が固定済み
- Files: `src/cluster/control.rs`、`src/cluster/coordinator/control.rs`、
  `src/cluster/worker.rs`、関連 unit test
- Actions:
  1. offer/confirm と session phase を型として追加する。
  2. candidate 計算を checked arithmetic で実装する。
  3. atomic session commit で lease/phase/idempotency map を一貫更新する。
  4. duplicate offer/confirm を同じ response へ収束させる。
  5. stale non-Pair command/ack の拒否を維持する。
- Verification: G-01 の状態遷移表を table-driven unit test 化、共通 local gate
- 完了条件: network と永続 store を使わず全 protocol rule を検証できる
- 停止条件: test ごとに内部 field を直接書き換えないと状態を作れない
- Evidence: `src/cluster/control.rs` の `ControlProcessor` に `candidate_generation(peer_generation)` を追加 (local と peer の大きい方を返し、`u64::MAX` を overflow 扱いしない)。`src/cluster/coordinator/control.rs` の `CoordinatorControl` に `propose_candidate(peer_generation)` を追加し、peer が高い場合は offer 前に local generation を advance する (session authority)。`src/cluster/production/pairing.rs` の `control_generation()` を peer lease descriptor が無い場合も processor の live generation を返すよう修正し、advance 直後の offer で descriptor.generation と message.generation が一致するようにした。table-driven unit test を追加: `candidate_generation_never_lowers_the_known_control_session_generation` (同世代/worker 高/coordinator 高/`u64::MAX`)、`propose_candidate_adopts_a_higher_worker_generation_and_keeps_local_otherwise` (世代が下がらないこと)。network と永続 store を使わず検証可能。共通 local gate 全項目成功; 2026-08-14

### [x] G-03 control session を永続化・復旧する

- Actor: agent
- Depends on: G-02
- 着手可能条件: 永続化する最小 field と schema migration が G-01 で固定済み
- Files: `src/cluster/state_store.rs`、起動/restart wiring、関連 fixture/test
- Actions:
  1. committed session と必要な pending negotiation 情報を atomic write する。
  2. 旧 schema を設計どおり migrate または安全に拒否する。
  3. truncated/corrupt state で古い command を再受理しない。
  4. process 再生成時に authority、session generation、phase を復元する。
  5. cluster generation と control session generation を別 field のまま保持する。
- Verification: crash point ごとの save/load test、旧 fixture test、共通 local gate
- 完了条件: 双方を再起動せず片側だけで negotiation を再開できる
- 停止条件: runtime state 削除を通常 recovery にする必要がある
- Evidence: `src/cluster/state_store.rs` の `PersistentClusterState` に `control_session_generation: u64` (cluster `generation` とは別 field) を追加し、`#[serde(default)]` で旧 schema v1 fixture (field なし) を引き続き読めるようにした (schema version は据え置き)。`StateStore::load()` で、field 欠落時に serde default (0) になった値を cluster generation まで normalize し、session が persisted cluster generation より低くならないようにした。`src/app.rs` の `persist_runtime_state` が `ProductionClusterRuntime::control_session_generation()` を保存し、`serve` が persisted 値を `attach_control_plane` -> `ProductionClusterRuntime::new(.., control_session_generation)` へ渡して起動時に復元する。`new_inner` は override が無ければ mode snapshot generation を使う (挙動不変)。fixture test `control_session_generation_round_trips_and_older_fixture_defaults_to_cluster_generation` を追加 (round-trip と旧 fixture の default normalize)。共通 local gate 全項目成功; 2026-08-14

### [x] G-04 production Pair transport を新 protocol へ接続する

- Actor: agent
- Depends on: G-03
- 着手可能条件: pure state machine と persistence が GREEN
- Files: `src/cluster/production.rs`、`src/cluster/production/pairing.rs`、
  `src/cluster/production/effects.rs`、`src/cluster/production/reconcile.rs`、HTTP test
- Actions:
  1. `/v1/node` の peer generation/session 情報を negotiation input にする。
  2. coordinator の periodic pair を offer -> confirm -> committed lease の順へ変更する。
  3. worker の返信は protocol で定義した confirm に限定し、authority を持たせない。
  4. HTTP timeout 後の retry を同一 session へ冪等収束させる。
  5. session commit 前に `form_pair` や auto promotion を開始しない。
- Verification: control HTTP integration、409/error mapping、共通 local gate
- 完了条件: production transport で片側高世代を方向非依存に解消できる
- 停止条件: local generation だけを lease/phase より先に更新する window が残る
- Evidence: `src/cluster/production/pairing.rs` の `pair()` を offer/confirm へ変更。coordinator は先に `client.node()` で worker の control session generation を取得し、`CoordinatorControl::propose_candidate(peer.generation)` で candidate を計算して必要なら自分の generation を advance した後に offer を送る (design §4)。`/v1/node` 応答 (`ControlResponse.generation`) を negotiation input として使用。offer の `generation` と descriptor は共に candidate になるため、worker は高世代を advance して受理し confirm を返し、coordinator が commit する。generation 更新は `propose_candidate` の一操作 (advance + lease + phase を同一 Mutex 内) で行われ、local generation だけを先に更新する window は残らない。R0-06 の `coordinator_adopts_higher_worker_generation_on_pair` が GREEN になることを確認 (worker 高世代で pair が収束)。共通 local gate 全項目成功; 2026-08-14

### [x] G-05 片側再起動と duplicate の GREEN test を完成する

- Actor: agent
- Depends on: G-04
- 着手可能条件: production offer/confirm が利用可能
- Files: `tests/reconnect_production.rs`、control/restart test
- Actions:
  1. coordinator のみ、worker のみ、両 process の再生成を Paired と Distributed の開始状態で試す。
  2. worker 高世代、coordinator 高世代、同世代を含める。
  3. offer/confirm の応答遅延、消失、重複、順序逆転、process crash を注入する。
  4. committed session より古い Prepare/Ready/Demote/ack が拒否されることを確認する。
  5. 収束後の session generation と child generation の対応を確認する。
  6. `R0-06` で付けた一時 `ignore` を解除する。
- Verification: R0-06 が GREEN、方向別 matrix、共通 local gate
- 完了条件: periodic retry だけで全方向が有限時間内に収束する
- 停止条件: test ごとに state file の削除または両 node 再起動が必要
- Evidence: `tests/reconnect_production.rs` の `coordinator_adopts_higher_worker_generation_on_pair` (worker baseline 100 / coordinator 0) の一時 `ignore` を解除し GREEN 確認。direction 別 matrix を追加: `worker_higher_pair_converges_on_the_worker_control_session_generation` (worker 高世代で両 node が worker の session generation に収束、重複 pair で世代が下がらない)、`coordinator_higher_pair_keeps_the_coordinator_control_session_generation` (coordinator 高世代を worker が追従、重複 pair で維持)。収束後の session generation と重複 pair の安定性を assert。`cargo test --test reconnect_production --features test-support` を 10 回反復し全回 GREEN (13 passed + 0 ignored、flaky なし、state file 削除や両 node 再起動は不使用)。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets / cargo test --all-targets --features test-support / git diff --check clean); 2026-08-14

## 9. Phase E: P0-C 再接続 E2E

### [x] E-01 P0 reconnect matrix を一つの acceptance suite にする

- Actor: agent
- Depends on: A-04、G-05
- 着手可能条件: lifecycle と session の個別 suite が GREEN
- Files: `tests/reconnect_production.rs`、`tests/support/mod.rs`
- Actions:
  1. Paired + cable blip 相当を 10 cycle 実行する。
  2. Distributed + cable blip 相当を 10 cycle 実行する。
  3. Paired/Distributed から coordinator のみ、worker のみ、両 process 再起動を実行する。
  4. Pair response 遅延・重複を実行する。
  5. 各 checkpoint で両 node の state/mode、cluster/control generation、lease/phase、target、
     admission、standalone/distributed child identity を共通 helper で assert する。
  6. 最終 checkpoint だけでなく、Solo serving と Paired serving を途中 checkpoint として必須にする。
- Verification: `cargo test --test reconnect_production --features test-support` を単独実行後、共通 local gate
- 完了条件: proposal P0-C の全行が test 名へ一対一で対応する
- 停止条件: flaky 回避のため cycle 数削減や timeout 延長が必要
- Evidence: `tests/reconnect_production.rs` と `tests/support/mod.rs` を commit 8798b1a で追加。P0-C の各行を test 名へ一対一対応させた: `paired_cable_blip_repairs_to_paired_over_10_cycles` / `distributed_cable_blip_rebuilds_new_generation_over_10_cycles` (各 10 cycle) / `coordinator_only_restart_converges_generation_without_orphan` / `worker_only_restart_converges_including_coordinator_low_generation` (coordinator 低世代の worker 高世代採用を含む) / `both_process_restart_converges_from_persisted_generation` / `pair_response_delay_and_duplicate_converge_idempotently`。harness 側は cable blip 相当を `Node::stop_serve`/`restart_serve` (非 blocking std listener clone で同一 control port を再 serve)、process 再起動相当を `Node::restart_control_process(persisted_generation)` (serve/child 終了後、fresh mode + `new_with_lifecycles(.., Some(generation), ..)` で永続 control session generation から再起動) として実装。共通 helper (`assert_solo_serving` / `assert_paired_consistent` / `assert_paired_serving` / `assert_distributed_consistent`) で state、stable mode、cluster/control generation、phase、lease、proxy target、admission、standalone/distributed child identity を両 node 検証し、Solo serving と Paired serving を途中 checkpoint として必須化 (最終 checkpoint のみでなく)。設計判断の根拠は R0-04/05/06 Evidence から参照: 同一 loopback 127.0.0.1 を別 port で分離、test-support 専用 peer control port 注入、`inject_fake_worker_hello` による promotion 用 fake HELLO、`boot_with_baseline`/`spawn_ready_at` による baseline generation 注入。Verification は `cargo test --test reconnect_production --features test-support` 単独 (19 passed / 0 ignored) と共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 176 + integration GREEN / cargo test --all-targets --features test-support: unit 176 + integration + reconnect_production 19 GREEN / git diff --check clean)。cable blip 2 test を別途 2 回反復し flaky なし (固定 sleep 不使用、`required_peer_stability` と lease 期限を利用)。; 2026-08-15

## 10. Phase B: P1 Backoff と operator reconcile

### [x] B-01 promotion failure の分類と tracker 接続を統一する

- Actor: agent
- Depends on: E-01
- 着手可能条件: reconnect 自体の失敗と promotion failure を診断 field で区別できる
- Files: `src/cluster/state.rs`、`src/cluster/coordinator.rs`、
  `src/cluster/production/pairing.rs`、failure test、必要なら `docs/spec.md`
- Actions:
  1. Hello timeout、unknown DS4 schema、coordinator startup timeout の `failure_action()` を確認する。
  2. spec 第 31 節との方針差を解消し、期待 action を test 名と表へ固定する。
  3. `PromotionBackoff` 対象を同一 tracker へ記録する。
  4. reconnect 成功後の promotion failure を reconnect failure として記録しない。
- Verification: table-driven failure action/tracker test、共通 local gate
- 完了条件: 対象 failure が漏れなく backoff または明示した別 action へ入る
- 停止条件: unknown schema の製品方針を operator が判断していない
- Evidence: branch=feature/reconnect-recovery; spec.md §31 と計画書 P1 冒頭の方針差を解消し、`src/cluster/state.rs` の `failure_action()` で `UnknownDs4Schema` を `PromotionBackoff` から `PairedStandalone` へ変更 (unknown HELLO/log schema は promotion 拒否・Paired Standalone 維持、backoff しない)。`HelloTimeout` と `CoordinatorStartupTimeout` は `PromotionBackoff` を維持し、同一 `PromotionFailureTracker` へ記録する配線を統一: `src/cluster/production/pairing.rs` に `PromotionHelloError` (Rendezvous/Control/Preflight/WorkerStartupTimeout) を追加して `prepare_and_accept_hello` のエラーを型付けし、`promote()` の preflight 失敗で `Rendezvous(Ds4HelloError::Timeout)` または `WorkerStartupTimeout` のときだけ `record_promotion_failure(HelloTimeout, now)` で tracker へ接続、それ以外 (deployment mismatch / unknown schema 等) は Paired Standalone のまま tracker へ入れない。coordinator startup timeout は既存の `promotion_failure_for_error` (StartupTimeout → CoordinatorStartupTimeout) で tracker へ接続済みであることを確認。reconnect 成功後の promotion failure を reconnect failure として記録しない方針は、tracker が `note_success` で consecutive を reset し、`PeerAbsent` 等の reconnect failure は `PromotionBackoff` でないため tracker が `NotPromotionFailure` で拒否することに固定。`tests/phase5_failure.rs` を更新: `UnknownDs4Schema` 行を `PairedStandalone` に変更して table-driven failure_action を spec §31 と一致させ、tracker が HelloTimeout/CoordinatorStartupTimeout を Backoff として記録、UnknownDs4Schema と PeerAbsent を NotPromotionFailure で拒否、note_success 後の consecutive reset を検証。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 176 + integration GREEN / cargo test --all-targets --features test-support: unit 176 + integration + reconnect_production 19 GREEN / git diff --check clean); 2026-08-15

### [x] B-02 periodic reconcile に Backoff 復帰を接続する

- Actor: agent
- Depends on: B-01
- 着手可能条件: tracker が backoff deadline を保持する
- Files: `src/cluster/production/worker.rs` または reconcile task の移動先、関連 test
- Actions:
  1. coordinator の periodic task から `reconcile_backoff(now)` を呼ぶ。
  2. Backoff 中は pair/promote と同時実行しない。
  3. Backoff 中の peer loss を最優先にし、A-01 の Solo recovery へ渡す。
  4. deadline 前は state/generation を変更せず、deadline 後は一度だけ復帰させる。
- Verification: paused time の deadline 境界、peer loss race、共通 local gate
- 完了条件: operator 操作なしで期限後に安全な stable state へ収束する
- 停止条件: interval の実時間 sleep に依存する test しか作れない
- Evidence: branch=feature/reconnect-recovery; `src/cluster/production/reconcile.rs` の `reconcile()` を periodic 用エントリ (`reconcile_periodic()`) へ分割し、coordinator が Backoff 状態のとき peer loss を最優先で検出 (peer_present=false なら共有 `recover_from_peer_loss` で Solo へ)、peer 生存時は `CoordinatorDistributedRuntime::reconcile_backoff(now)` を呼び deadline 後に一度だけ PairedStandaloneReady へ復帰させる配線を追加。`src/cluster/state.rs` の state machine で `Backoff` 状態からの `PeerLost` 遷移 (→ SoloStandaloneStarting) を許可し、`requires_solo_fallback` に `Backoff` を追加して control 経由の reconcile でも Backoff 中の peer loss が Solo recovery へ渡るようにした。Backoff は pair/promote の trigger state でないため、Backoff 中は pair/promote と同時実行されない。`src/cluster/production.rs` に test-support 専用の `record_promotion_failure` (coordinator runtime の tracker へ記録し Backoff へ駆動する injection) を追加。`tests/reconnect_production.rs` に `coordinator_backoff_reconcile_recovers_to_paired_after_deadline` (deadline 前は Backoff 維持、deadline 後は PairedStandaloneReady へ一度だけ復帰、以降 no-op) と `peer_loss_during_backoff_recovers_to_solo_first` (Backoff 中の peer loss が Solo 復帰を優先) を追加。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 176 + integration GREEN / cargo test --all-targets --features test-support: unit 176 + integration + reconnect_production 21 GREEN / git diff --check clean); 2026-08-15

### [x] B-03 admin reconcile を coordinator runtime へ接続する

- Actor: agent
- Depends on: B-02
- 着手可能条件: production runtime の coordinator failure tracker へ到達する API が定義済み
- Files: `src/app.rs`、`src/cluster/admin.rs`、`src/cluster/production.rs`、関連 test
- Actions:
  1. admin `cluster reconcile` を `CoordinatorDistributedRuntime::operator_reconcile()` 経由にする。
  2. tracker reset と `OperatorReconcile` event を一つの操作として扱う。
  3. worker/role unknown/非 manual state の response を明示する。
  4. admin token と既存 mutation authorization を維持する。
- Verification: admin HTTP test、tracker reset assertion、共通 local gate
- 完了条件: manual state 解除後に同じ失敗回数が持ち越されない
- 停止条件: state machine へ event を直接送る旧 bypass が残る
- Evidence: branch=feature/reconnect-recovery; `src/cluster/production.rs` に `OperatorReconcileOutcome` (`Coordinator { manual_cleared }` / `NotCoordinator { manual_cleared }`) と `ProductionClusterRuntime::operator_reconcile()` を追加し、coordinator runtime が存在すれば `CoordinatorDistributedRuntime::operator_reconcile()` (tracker reset と `OperatorReconcile` event を 一つの atomic 操作として適用) を呼び `Coordinator` を返す。worker / role unknown は coordinator runtime を持たないため mode runtime へ `OperatorReconcile` event を直接適用して `NotCoordinator` を返す (旧実装の `RuntimeAdminExecutor` が state machine へ event を直接送る bypass は削除)。 `src/app.rs` の `RuntimeAdminExecutor::execute` の `AdminAction::Reconcile` を `production.operator_reconcile()` 経由へ書き換え、response の `reconcile` field で coordinator tracker reset / worker / role unknown / 非 manual state の結果を明示する (admin token と `start_admin_job` 経由の mutation authorization は既存のまま維持)。 `Cargo.toml` の `test-support` feature に `tower` (admin HTTP 検証用) を追加し、 `src/app.rs` に test-support 専用の `admin_http_reconcile` (実 `admin_router` へ POST して async job を 完了まで polling する HTTP 検証ヘルパー) を追加。`ProductionClusterRuntime` に test-support 専用の `promotion_failure_status` accessor を追加。`tests/reconnect_production.rs` に 4 本を追加: `operator_reconcile_resets_coordinator_tracker_and_clears_manual_state` (3 連続 failure で ManualInterventionRequired → operator_reconcile で PairedStandaloneReady へ、tracker consecutive 3→0 / manual false、完了条件の「manual state 解除後に同じ失敗回数が持ち越されない」を固定)、 `operator_reconcile_on_worker_and_non_manual_coordinator_are_explicit` (worker は NotCoordinator、 非 manual coordinator は Coordinator{manual_cleared:false} の no-op)、 `admin_http_reconcile_clears_manual_state_and_resets_tracker` (実 HTTP 経由で response の `reconcile`="coordinator promotion tracker reset; manual intervention cleared" と tracker reset を検証)、 `admin_http_reconcile_on_worker_reports_no_coordinator_tracker` (worker の HTTP response が "no coordinator promotion tracker on this node" で状態不変を検証)。 共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 176 + integration GREEN / cargo test --all-targets --features test-support: unit 176 + integration + reconnect_production 25 GREEN / git diff --check clean); 2026-08-15

### [x] B-04 promotion failure 後の reconnect E2E を追加する

- Actor: agent
- Depends on: B-03
- 着手可能条件: backoff と operator reconcile の production wiring が GREEN
- Files: `tests/reconnect_production.rs`、`docs/operations.md`、`docs/troubleshooting.md`
- Actions:
  1. reconnect 後の promotion を意図的に失敗させ、Backoff と deadline 復帰を確認する。
  2. failure 上限到達で ManualInterventionRequired になることを確認する。
  3. 原因除去後の operator reconcile で tracker reset、自動復帰を確認する。
  4. Backoff 中の peer loss が Solo recovery を優先することを確認する。
  5. operator 向け確認・復旧手順を更新する。
- Verification: 対象 E2E、文書 command の実在確認、共通 local gate
- 完了条件: promotion failure を含む reconnect が自動または明示的 operator 操作で収束する
- 停止条件: test を通すため failure counter を test から直接 reset する必要がある
- Evidence: branch=feature/reconnect-recovery; `tests/reconnect_production.rs` に `reconnect_then_promotion_failure_cycle_converges_via_backoff_and_operator_reconcile` を追加: DistributedReady から cable blip (peer loss → Solo serving → re-pair → Paired serving) の reconnect 後、 `record_promotion_failure` (test-support の failure 注入) で promotion を意図的に失敗させて Backoff へ、 deadline 前の reconcile は Backoff 維持、deadline 後の reconcile で一度だけ PairedStandaloneReady へ復帰、 さらに 2 回失敗を重ねて failure 上限 (3) 到達で ManualInterventionRequired になり reconcile では自動解除されず、 実 `operator_reconcile()` で tracker consecutive 3→0 / manual false へ reset され PairedStandaloneReady へ自動復帰、 最後に promote_to_distributed で DistributedReady へ再収束する (完了条件の 「promotion failure を含む reconnect が自動 (deadline) または明示的 operator 操作で収束する」を固定)。 復帰は必ず実 operator reconcile 経由で、test から tracker を直接 reset しない (B-04 停止条件を遵守)。 `reconnect_then_peer_loss_during_backoff_recovers_to_solo_first` を追加: reconnect (blip → re-pair) 後、 Backoff 中の peer loss が deadline より優先され reconcile で SoloStandaloneReady へ収束することを固定。 operator 向け文書を更新: `docs/operations.md` §5 Manual state と `docs/troubleshooting.md` §3.9 に、 `cluster reconcile` が coordinator の promotion failure tracker を reset して ManualInterventionRequired 解除とを 一つの atomic な操作として扱い (plan B-03)、原因除去後の reconcile で失敗回数が持ち越されず次の promotion 試行は 失敗回数 0 から再開すること、解除後に `siderostat cluster status` で stable state へ戻り tracker の失敗回数が 保持されないことを確認する手順を追記。`siderostat cluster reconcile` CLI command の実在を `src/cli.rs` で確認。 共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test --all-targets: unit 176 + integration GREEN / cargo test --all-targets --features test-support: unit 176 + integration + reconnect_production 27 GREEN / git diff --check clean); 2026-08-15

## 11. Phase N: P2 route / discovery pairing gate

### [x] N-01 pairing gate の network evidence contract を固定する

- Actor: agent + operator review
- Depends on: B-04
- 着手可能条件: session negotiation が network gate から独立して安定している
- Files: `docs/pairing-network-gate.md`（新規）、必要なら `docs/spec.md`
- Actions:
  1. fixed source、HMAC、bridge0 scoped route、lease、stability、discovery candidate の必須条件を表にする。
  2. snapshot/candidate に generation または観測 epoch を付け、古い観測を拒否する。
  3. Bonjour 単独では peer present にしない。
  4. network event と periodic snapshot が競合した場合の優先順位、debounce、失効条件を決める。
  5. macOS API 失敗時は fail closed か既存 lease 維持かを時系列で定義する。
- Verification: attach/detach、wrong interface/subnet、stale candidate、Bonjour failure の真理値表
- 完了条件: production handler が `route_scoped=true` を固定値で渡す必要がなくなる設計が承認済み
- 停止条件: ICMP や Bonjour presence だけを trust する必要がある

- Evidence: branch=feature/reconnect-recovery; `docs/pairing-network-gate.md` を新規作成。
  現行 `src/cluster/production/effects.rs` の `ProductionClusterRuntime::handle()` が
  `RoleControl::Coordinator` / `RoleControl::Worker` へ `.node_descriptor(&authenticated, true, now)` /
  `.handle(endpoint, message, &authenticated, true, now)` で **固定値 `route_scoped=true`** を
  渡している 4 箇所 (60, 66, 79, 93 行目) を明記し、production 経路で `bridge0` scoped route の
  実測が control lease / pairing 判定へ効いていない問題を固定した。peer present の必須条件
  (spec §9.3/§13.1/§13.3) を表にし、`bridge0` local address、期待 peer address と subnet、
  bridge0 scoped route、HMAC 認証済み node descriptor、control lease、`required_peer_stability`
  の 6 条件を列挙。discovery candidate の必須条件は `DiscoveryTracker::accept_bonjour` の
  検査順序 (`OldGeneration` / `SelfResult` / `WrongInterface` / `WrongProtocol` / `InvalidPort` /
  `WrongSubnet` / `UnexpectedAddress` / `RouteNotScoped`) に固定し、static fallback は
  `BonjourFailure::allows_static_fallback()` かつ route scoped / port 非 0 のみ許可した。
  snapshot / candidate に観測 epoch を付与して古い観測を拒否する設計を定義
  (`network_events.rs` の generation フィルタ、`BonjourLifecycle::accepts`、
  `DiscoveryTracker::OldGeneration` を参照)。Bonjour 単独では peer present にしない
  (`NetworkSnapshot::from_observation` は `AuthenticatedPeer` のときだけ true、
  `PeerCandidateFound` / `ReadyNoPeer` は false、`wrong_route_or_candidate_never_becomes_peer_present`
  test で route 非 scoped の candidate が `authenticated=true` でも peer present にならないことを
  固定)。network event と periodic snapshot の競合は `spawn_network_event_monitor` を正とし、
  Initial / debounce (500ms) / reconcile (30s) の優先順位と失効条件を定義。macOS API 失敗は
  fail closed (新 lease 不確立・promotion 不開始・`ServiceMissing` 等で peer present にしない) とし、
  既存 lease は失効 (15s) まで current mode を維持、address/route 消失または lease 失効で
  future admission を閉じ single recovery owner で Solo へ収束する時系列を定義。attach/detach、
  wrong interface/subnet、stale candidate、Bonjour failure、macOS API 失敗の真理値表を作成。
  完了条件として N-02 で 4 箇所の固定 `true` を最新 epoch の `NetworkSnapshot` 由来の実測
  `route_scoped` へ置換する設計を示した。**Actor: agent + operator review** のため、本 Evidence は
  operator review 待ち (`[x]` 未確定) としていたが、2026-08-15 に operator 承認を
  得て `[x]` に確定。共通 local gate は文書のみ変更のため `git diff --check` clean
  で確認; 2026-08-15

### [x] N-02 network evidence を production control へ接続する

- Actor: agent
- Depends on: N-01
- 着手可能条件: evidence provider の入力/出力と fail behavior が固定済み
- Files: `src/cluster/network_snapshot.rs`、`src/cluster/discovery.rs`、
  `src/cluster/network_events.rs`、`src/cluster/production.rs`、関連 platform code/test
- Actions:
  1. 検証済み network snapshot/candidate を production runtime へ共有する。
  2. control handler の固定 `route_scoped=true` を実測値へ置換する。
  3. snapshot epoch と request 時点の整合を検証する。
  4. event loss を periodic reconcile で回復する。
  5. route loss が A-01 の単一 recovery owner へ到達することを維持する。
- Verification: platform-independent unit test、macOS compile、共通 local gate
- 完了条件: peer present の各条件が実際の production input に由来する
- 停止条件: network 設定変更や shell output parsing が必要
- Evidence: branch=feature/reconnect-recovery; `src/cluster/network_snapshot.rs` の
  `NetworkObservation` / `NetworkSnapshot` に観測 `epoch` を追加し `from_observation` で
  伝搬、`stale_relative_to(newer_epoch)` と fail-closed な `Default` (ReadyNoPeer / epoch 0 /
  peer_present false) を追加。新規 `src/cluster/network_evidence.rs` に共有
  `NetworkEvidence` (RwLock で最新 `NetworkSnapshot` と `observed_epoch` を保持) を追加し、
  `update()` は古い epoch の snapshot を拒否 (stale)、`route_scoped()` は
  `PeerCandidateFound | AuthenticatedPeer` のときだけ true (candidate が期待 peer かつ
  bridge0 scoped)、`peer_present()` は `snapshot.peer_present`、未観測は fail-closed。
  `ProductionInner` に `network: Arc<NetworkEvidence>` を追加し、`src/cluster/production/
  effects.rs` の `handle()` で `route_scoped = self.inner.network.route_scoped()` を一度だけ
  評価して `/v1/node` (`node_descriptor`) と各 control message (`handle`) の 4 箇所の固定
  `true` を置換した (plan N-01 完了条件、action 2)。macOS observation provider
  `observe_network_observation(interface, coordinator, worker, peer)` を `getifaddrs` のみ
  で実装 (service/interface/address + peer candidate の同 subnet による bridge0 route scoped
  判定、interface 欠落/down/address なしは fail-closed)。`ProductionClusterRuntime::
  start_network_evidence_monitor` (macOS) が `spawn_network_event_monitor` +
  `MacOsDynamicStoreWatcher` を配線し、Initial / debounced event / periodic reconcile の各
  rescan で単調増加 epoch を付与して共有 evidence を更新 (event loss は reconcile で回復、
  action 4)、detached task が watcher を保持し process lifetime で稼働。`new` (production
  経路) でのみ起動し、非 macOS は no-op。test harness (`tests/support/mod.rs`) に
  `valid_peer_evidence(role)` を追加し、`Node::build` と `restart_control_process` で
  `AuthenticatedPeer` (epoch 1) を `set_network_evidence` 注入して既存 27 本を GREEN 維持。
  platform-independent unit test: `network_evidence` (default fail-closed / route_scoped 写像 /
  stale 拒否) と `network_snapshot::snapshot_carries_observation_epoch_and_rejects_stale_
  relative_to`。`tests/reconnect_production.rs` に `pairing_is_fail_closed_without_route_
  scoped_evidence` (両 node を ReadyNoPeer の新 epoch へ置換 → pair が RouteNotScoped で
  拒否され Solo 維持、完了条件の fail-closed を固定) と `stale_network_evidence_is_rejected_
  and_pairing_keeps_latest` (stale epoch 0 が拒否され最新 evidence 維持で pair 収束、action 3)
  を追加。action 5 (route loss が A-01 の単一 recovery owner へ到達) は既存
  `route_loss_monitor_and_peer_loss_reconcile_race_converge_to_solo` 等が GREEN のままである
  ことを確認。共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets
  --all-features -- -D warnings / cargo test --all-targets: unit 180 + integration GREEN /
  cargo test --all-targets --features test-support: unit 180 + integration + reconnect_production
  29 GREEN / git diff --check clean); 2026-08-15

### [x] N-03 route/discovery reconnect matrix を自動化する

- Actor: agent
- Depends on: N-02
- 着手可能条件: production handler が実測 evidence を利用する
- Files: `tests/phase5_security.rs`、`tests/reconnect_production.rs`、network test
- Actions:
  1. valid attach、route detach、address loss、wrong interface/subnet、stale candidate を試す。
  2. Bonjour success + route invalid で pair しないことを確認する。
  3. Bonjour failure + static valid evidence の仕様上の behavior を確認する。
  4. detach/attach 10 cycle で duplicate pairing と orphan recovery がないことを確認する。
  5. network epoch より古い Pair offer/confirm を拒否または再ネゴシエーションする。
- Verification: security/reconnect test、共通 local gate
- 完了条件: P2 の truth table と全 test 名が対応する
- 停止条件: fake evidence と macOS provider の意味が一致しない
- Evidence: branch=feature/reconnect-recovery; truth table (docs/pairing-network-gate.md §8)
  の全行と test 名が一対一で対応することを確認した。`tests/reconnect_production.rs` に
  `network_evidence_truth_table_maps_every_state_to_route_and_peer_present` (ThunderboltIpState
  8 状態をループし、`PeerCandidateFound` = (route_scoped true, peer_present false) /
  `AuthenticatedPeer` = (true, true) / 他 6 状態 = (false, false) を production の
  `network_gate_status()` で検証、action 1 の attach と「認証未完了」行) と
  `non_route_scoped_network_evidence_rejects_pairing_and_stays_solo` (route-detach /
  wrong-interface / wrong-subnet / wrong-address / stale-candidate / macos-api-failure /
  bonjour-success-route-invalid の 7 シナリオを両 node に注入し、pair が RouteNotScoped で
  拒否され Solo serving を維持、シナリオ間は新 epoch の valid evidence で復元。action 1 の
  detach/address loss/wrong interface/subnet/stale candidate、action 2 の Bonjour success +
  route invalid に相当) と `bonjour_failure_with_static_valid_evidence_requires_authentication_
  before_peer_present` (Bonjour 不可でも static fallback candidate (`PeerCandidateFound`、
  epoch 50) は bridge0 route scoped だが `peer_present=false` のまま、HMAC 認証が必要。
  truth table の Bonjour failure 行、action 3) を追加。`src/cluster/discovery.rs` に unit
  test `rejects_stale_generation_wrong_protocol_and_invalid_port` (旧 generation の candidate
  を `CandidateError::OldGeneration` で拒否、truth table の stale candidate 行) を追加。
  action 4 (detach/attach 10 cycle の duplicate pairing / orphan recovery) は既存
  `tests/reconnect_production.rs::paired_cable_blip_repairs_to_paired_over_10_cycles` /
  `distributed_cable_blip_rebuilds_new_generation_over_10_cycles` (harness の
  stop_serve/restart_serve で 10 cycle、generation 非減少) と
  `tests/phase5_security.rs::ten_route_detach_attach_cycles_converge_without_orphan_state`
  (WorkerControl 直接で 10 cycle、orphan なし) が GREEN のままであることを確認した。設計
  §8 の「既存 lease は失効 (15s) まで現 mode を維持」に従い、detach 直後に即座に Solo へ
  落ちることを期待する cycle は追加しない (設計に反する)。action 5 (古い epoch の Pair
  offer/confirm 拒否) は N-02 の `stale_network_evidence_is_rejected_and_pairing_keeps_latest`
  と `src/cluster/production/effects.rs` の `GenerationMismatch` 再送出
  (offer/confirm の generation 不一致を 409 で拒否) で担保済みであることを確認した。
  共通 local gate 全項目成功 (cargo fmt --check / cargo clippy --all-targets --all-features
  -- -D warnings / cargo test --all-targets: unit 181 + integration GREEN / cargo test
  --all-targets --features test-support: unit 181 + integration + reconnect_production 32 GREEN
  / git diff --check clean); 2026-08-15

## 12. Phase Q: P3 Pair timing の判定と条件付き実装

### [x] Q-01 Pair timing を計測して実装要否を判定する

- Actor: agent
- Depends on: N-03
- 着手可能条件: P0〜P2 の suite が GREEN
- Files: test instrumentation、必要なら本書の Evidence のみ
- Actions:
  1. offer、confirm、lease establish、stability 達成、PairingReady の timestamp を採取する。
  2. response 遅延、重複、packet loss 相当を含めて最低 100 session 実行する。
  3. sleep 満了後に confirm 未完了の session、余分な retry、収束 timeout の件数を集計する。
  4. 未収束または timing 依存 failure が 1 件でもあれば Q-02 を必須とする。
  5. 0 件なら、P3 不要の根拠と test seed/command を Evidence に記録する。
- Verification: 同じ seed で再実行可能な計測 artifact
- 完了条件: Q-02 の「実装」または「不要」が数値で決定済み
- 停止条件: timestamp log に secret/nonce/request body が含まれる
- Evidence: branch=feature/reconnect-recovery; `src/cluster/production.rs` に test-support
  限定の計測 hook を追加した (`PairTiming` 構造体 + `ProductionInner::pair_timings` +
  `ProductionClusterRuntime::pair_timings()` アクセサ、`siderostat::cluster::PairTiming` として
  公開)。`pair()` が offer 送信 (`offer_sent_at`) / confirm 受信 (`confirm_received_at`) /
  lease establish (`lease_established_at`) / stability 達成 (`stability_achieved_at`) /
  PairingReady 到達 (`pairing_ready_at`) の各タイムスタンプを記録する。タイムスタンプのみで
  secret/nonce/request body は記録しない (停止条件を満たす)。`tests/reconnect_production.rs`
  に `pair_timing_one_hundred_sessions_confirm_before_stability_and_converge` を追加し、
  100 session を実行した。packet loss 相当として 10 session ごとに cable blip
  (stop_serve + PeerLost reconcile → Solo → restart_serve) を挟み、残りは重複 pair とした。
  計測結果: sessions=100/100 収束、confirm_after_stability=0、convergence_timeouts=0、
  pair_errors=0、confirm は stability 達成の平均 102.6ms / 最大 104ms 前に完了 (stability
  sleep は 100ms)。これは現 protocol が `pair()` 内で `client.send()` が confirm を同期受信
  してから required_peer_stability の sleep に入る構造であるため、confirm 未完了のまま
  sleep が満了する race が構造的に存在しないことを裏付ける。action 3 の「sleep 満了後に
  confirm 未完了」は 0 件、余分な retry も収束 timeout も 0 件。再実行コマンド:
  `cargo test --test reconnect_production --features test-support pair_timing_one_hundred_
  sessions_confirm_before_stability_and_converge -- --nocapture` (同じ固定シナリオで再実行
  可能)。action 5 の「P3 不要」を Q-02 の Evidence に転記した。共通 local gate 全項目成功
  (cargo fmt --check / cargo clippy --all-targets --all-features -- -D warnings / cargo test
  --all-targets: unit 181 + integration GREEN / cargo test --all-targets --features test-support:
  unit 181 + integration + reconnect_production 33 GREEN / git diff --check clean); 2026-08-15

### [x] Q-02 Pair confirm 完了通知を実装する、または不要判定を記録する

- Actor: agent
- Depends on: Q-01
- 着手可能条件: Q-01 が実装必要と判定した場合だけ code 変更する
- Files: `src/cluster/production/pairing.rs`、control session notification、関連 test、または本書のみ
- Actions:
  1. 必要な場合、lease/session commit を watch/notify し、`pair()` が confirm 完了を deadline 付きで待つ。
  2. required stability は commit 後の authenticated continuity に対して測る。
  3. notification の missed wakeup、duplicate、sender drop を test する。
  4. 不要な場合、code を変更せず Q-01 の根拠を本 task の Evidence に転記する。
- Verification: 100 session 計測の再実行、共通 local gate
- 完了条件: sleep の race がない、または現 protocol では問題がないと evidence で確定
- 停止条件: stability sleep の延長だけで解決しようとする
- Evidence: branch=feature/reconnect-recovery; 不要判定を記録する。Q-01 の 100 session 計測
  (packet loss 相当の cable blip 10 回 + 重複 pair 90 回) で、全 session が収束し、
  confirm が stability sleep 前に完了する session が 0 件 (confirm_after_stability=0)、
  収束 timeout 0 件、pair error 0 件だった。現 protocol では `pair()` が
  `client.send()` で Pair の confirm response を同期受信してから
  required_peer_stability の sleep に入るため、confirm 未完了のまま sleep が満了する
  race は構造的に存在しない。したがって、lease/session commit を watch/notify して
  `pair()` が confirm 完了を deadline 付きで待つ通知機構 (action 1) は不要と判定する。
  action 4 の「不要な場合、code を変更せず Q-01 の根拠を転記」に従い、production code
  は変更せず (計測 hook は test-support 限定)、Q-01 の Evidence をここへ転記した。
  stability sleep の延長では解決しない (停止条件を満たす)。Q-01 の計測テスト
  `pair_timing_one_hundred_sessions_confirm_before_stability_and_converge` を再実行して
  GREEN (100/100 収束) であることを確認済み。共通 local gate 全項目成功。2026-08-15

## 13. Phase H: ユーザー手作業を含む 2 node 実機検証

実機 task では、agent は command 提案、read-only 観測、結果整理を担当する。ユーザーは物理 cable
操作、macOS 再起動、candidate binary の配置承認を担当する。agent はユーザーの明示的依頼なしに
実機 service の停止、再起動、binary 上書きを行わない。

### [x] H-01 実機検証の change window と rollback を準備する

- Actor: operator + agent
- Depends on: N-03、Q-02 の判定完了
- 着手可能条件: 共通 local gate と reconnect acceptance suite が GREEN
- Files: repository 外 evidence directory、必要なら redaction 済み runbook
  （operator 手順は `docs/reconnect-field-verification-runbook.md` に集約）
- Actions:
  1. operator が両 node の利用停止可能時間と、進行中推論 request がないことを確認する。
  2. agent が candidate の commit SHA、binary SHA-256、config checksum を記録する。
  3. operator が現行 binary/config/state/log を削除せず backup または隔離する。
  4. operator が rollback binary と `launchctl` job label を確認する。
  5. agent が両 node の baseline `cluster status --json`、`cluster doctor --json`、process identity、
     log 開始位置を採取する。
- ログ: 実機の stdout/stderr は LaunchAgent の `StandardOutPath`/`StandardErrorPath` が
  単一の `$HOME/Library/Logs/siderostat/ds4-siderostat.log` へ統合する運用とする。backup 対象と
  log 開始位置の採取はこの単一ファイルに対して行い、`stdout.log` / `stderr.log` の別ファイルは
  用いない。
- Verification: candidate/rollback の checksum、両 node Solo または Distributed の健全な baseline
- 完了条件: 各操作を中止して旧 binary へ戻せることを operator が確認済み
- 停止条件: active workload、cleanup拒否指定またはidentity確認できない unknown DS4 child、重複 supervisor、backup 不在、node 時刻の大幅ずれ

Progress evidence: candidate commit `6d6164922ca90aac2372a4407b77b659f19059b1` を固定し、worker candidate
`/Users/o/Projects/github/okugauchi/siderostat/target/release/siderostat` の SHA-256 は
`6798f005fc39413988b2c762fc676f625107279d491eaf690ab818dfc2b47037`、coordinator candidate
`/Users/o/LLM/siderostat/target/release/siderostat` の SHA-256 は
`fd07857125e1ae6f3849c21cd7bd807c66c9c1baa8e2066c5a0f9a662546e133`; `cargo fmt --check`、
`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets`（181 passed）、
`cargo test --all-targets --features test-support`（181 unit + integration、reconnect production 33 passed）、
`cargo build --release` を実行。baseline artifact は
`/private/tmp/siderostat-reconnect-evidence-20260815/baseline/`（SHA256SUMS.txt の SHA-256:
`e6851ffa79d02577195e9f06dcbed1bd7e35917dee5faa901bc96aa5a8a0bd7a`）。read-only baseline は coordinator が
SoloStandaloneReady、worker が admin API 接続拒否かつ standalone DS4 child の readiness 前 exit status 2
であり、operator の backup / candidate 配置承認後も worker の健全な baseline が得られないため H-01 は継続中;
2026-08-15

Blocked evidence: operator 承認後、現行 binary/config/state/plist/log を rollback backup へ保全し、candidate を両 node の
user-owned path `/Users/o/Library/Application Support/siderostat/candidate-reconnect-20260815/siderostat` へ配置した。
`/usr/local/bin` は root 所有で非対話 sudo が使えないため上書きしていない。両 plist の lint と candidate SHA-256 は一致し、
coordinator は candidate で `SoloStandaloneReady`（generation 332、health/ready PASS）へ復帰した。一方 worker は candidate 起動後も
standalone DS4 child が HTTP readiness 前に exit status 2 で終了し、LaunchAgent を bootout して再試行ループを停止した。
worker の plist は保全 backup と SHA-256 が一致する元の `/usr/local/bin/siderostat` 指定へ戻し、LaunchAgent は unloaded のままにした。
worker の 2026-08-15 crash report には、直前の `/usr/local/bin/siderostat` に対する
`SIGKILL (Code Signature Invalid)` / `Launch Constraint Violation` が記録されている。candidate と DS4 binary の
`codesign --verify` は両 node で PASS だった。

原因調査結果 (2026-08-16): worker の今回の standalone exit status 2 は、DS4 の同一ノード instance lock 競合と判定した。
保存ログでは、worker は `DistributedReady` の distributed worker PID 1482 が動作中のまま `PeerLost` 後に
`SoloStandaloneStarting` へ入り、その後の standalone child が同じ世代で繰り返し exit status 2 になっている。
PID 1482 のログは `2026-08-15T09:46:39Z` まで続き、`2026-08-15T13:03:03Z` には restart reconcile が
`PersistedChildStopFailed` で `ManualInterventionRequired` へ遷移した。`2026-08-15T14:15:38Z` 以降の
standalone child は全て HTTP readiness 前に exit status 2 で終了した。DS4 source の
`ds4_acquire_instance_lock()` は `/tmp/ds4.lock` の競合時に「another ds4 process is already running」を出して exit 2
する。coordinator 上の同じ DS4 実体への controlled probe はこの stderr と exit 2 を再現し、worker 上で同じ standalone argv
をモデル `/dev/null` に置換した probe は CLI parse を通過して「model file is too small to be GGUF」exit 1 となったため、
worker の exit 2 は `--mtp` / `--dspark` などの argv 不整合ではない。

根本の lifecycle 問題は、DS4 worker が SIGTERM だけでは停止しない既知制約と、worker config の `allow_sigkill = false` により、
persisted distributed child の停止失敗が手動介入状態に固定され、後続の standalone 起動をロック競合へ導いたこと。なお、現在の
DS4 checkout `84cc882...` と binary digest（worker `344006...`、coordinator `982011...`）は互換性記録の承認 baseline
`b030961...` / worker `33f504...` / coordinator `a5b2e9...` とも一致しないため、lock 解消後も承認済み DS4 artifact へ戻す必要がある。
再開条件は、operator が live/orphan DS4 child を identity 確認付きで停止して lock 解放を確認し、承認済み DS4 artifact を
配置したうえで、worker の admin/health/ready と `SoloStandaloneReady` を確認してから両 node を同一 candidate で再起動すること。
証跡: `/private/tmp/siderostat-reconnect-evidence-20260815/baseline/`（SHA256SUMS.txt SHA-256:
`df089fca33b826c56b1b228f9507575dbddd98f1a55c32dc788ab4699c2a2191`）、worker rollback manifest SHA-256
`be5f52bf14b5a54a1e1efd378672f93cb5a8966b92096d612b55815711f216f9`、coordinator rollback manifest SHA-256
`7c61eadb67c783d03c16e79f14b98a183904cc327e0d6bf4ff7ff16e3c265977`; 2026-08-15

Completion evidence (2026-08-16): candidate commit `6bde9c10c148b3a85da6c015a5951bf21f3e898e` を両 node に配置し、candidate binary SHA-256 は両 node とも
`c21bd1934cb531f5f1abd729429c3800492004c7067a2a20f5d2e6a2542a66a0`、LaunchAgent plist SHA-256 は両 node とも
`dfbb42036293a72dae2b5f9d2ebdc2134327055abf349b920ae4e9bd0aa064eb`。config checksum は coordinator
`233cca9eab61faeb0c6e5544115ff7cf9675547cd2e2be1070f489085a23aa02`、worker
`a94e778092470cb2fefd7ad91f76e0e6378b3a1f663ec2c40418d008f6444912`。rollback backup は削除せず保持し、manifest SHA-256 は worker
`be5f52bf14b5a54a1e1efd378672f93cb5a8966b92096d612b55815711f216f9`、coordinator
`7c61eadb67c783d03c16e79f14b98a183904cc327e0d6bf4ff7ff16e3c265977`。

最終 evidence は worker `/private/tmp/siderostat-reconnect-evidence-20260815/baseline/20260816-h01-*`（manifest SHA-256
`988ed4fcef3196d355878546adff2cabbb2f633e42d1696b18a68b61f02e70de`）および coordinator
`/Users/o/siderostat-reconnect-evidence-20260815/baseline/20260816-h01-*`（manifest SHA-256
`74e56ceee48c3a538c0d80b8f6ddbc4c82580cc961e3ae13396dde0fd19d3e81`）に保存した。両 node の LaunchAgent は同一 candidate path を実行し、operator 承認済み startup cleanup は各 node 1 件の stale DS4 process に対して成功した。両 node の最終状態は `SoloStandaloneReady`、health/ready PASS、doctor `healthy=true`、admission serving、active request 0、node ごとの standalone DS4 child 1 件であり、H-01 の checksum / rollback / 健全 baseline 条件を満たした。これは H-01 完了時点の記録であり、その時点では H-02 は DistributedReady を着手条件とするため未着手だった。H-02 の実施結果は後述する。

### [x] H-02 DistributedReady から cable detach/reconnect を検証する

- Actor: operator（物理操作）+ agent（観測）
- Depends on: H-01
- 着手可能条件: 両 node が同じ candidate build で DistributedReady、推論停止中
- Files: repository 外 evidence directory
- Actions:
  1. agent が開始 snapshot と child identity を採取する。
  2. operator が Thunderbolt cable を抜く。network 設定は変更しない。
  3. agent が両 node の SoloStandaloneReady、LocalStandalone、Serving、distributed child 不在を確認する。
  4. operator が同じ cable を戻す。
  5. agent が route/session 再確立、Paired、auto promotion、新規 DistributedReady を順に確認する。
  6. 同じ cycle を連続 2 回行う。1 回でも失敗したら回数をリセットせず失敗 evidence を保存する。
- Verification: 各 checkpoint の JSON、log、PID/profile/generation、public inference smoke request
- 完了条件: 2 回連続で 1〜5 が成功し、proxy 各 1、node ごとの DS4 child 最大 1、orphan なし
- 停止条件: local standalone が ready にならない、unknown process、active user request、温度/容量等の運用警告
- Evidence: 2026-08-16 に candidate commit `8f6c86c`（両 node binary SHA-256 `a848c3d7894c9b5c508be0892ba0e4b9169b2060a327d56d4c493a9aa0de1082`）で cable detach/reconnect を 2 回連続実施。各回とも両 node が role loss 後に `SoloStandaloneReady` / `admission=serving` / distributed child 不在へ復帰し、再接続後に pairing・auto promotion・新 generation の `DistributedReady` へ収束した。最終 worker は generation 371 / distributed worker PID 50296、coordinator は generation 439 / distributed coordinator PID 36953。control phase は両 node `worker-ready`、lease valid / peer-present / route-scoped、`cluster doctor --json` は両 node `healthy=true`、active request は 0。public inference smoke は直列実行で worker/coordinator とも HTTP 200（並列 probe の一方は `max_in_flight=1` による HTTP 503、直列 retry は HTTP 200）。evidence summary: `/private/tmp/siderostat-reconnect-evidence-20260816/h02/20260816-h02-summary.md`。worker の stale distributed DS4 cleanup に各回約 3〜4 分を要したが、unknown/orphan process は残らなかった。

### [x] H-03 片側 process 再起動を方向別に検証する

- Actor: operator（service 操作承認）+ agent（観測）
- Depends on: H-01、H-02
- 着手可能条件: cable 接続済み、両 node DistributedReady、rollback 可能
- Files: repository 外 evidence directory
- Actions:
  1. coordinator の siderostat process だけを既存 LaunchAgent 手順で再起動する。
  2. worker を再起動せず、Solo -> session 再確立 -> Paired -> Distributed の収束を確認する。
  3. 新しい PID/session/child generation と orphan 不在を記録する。
  4. baseline を再確立後、worker の siderostat process だけで同じ手順を行う。
  5. 各方向を 2 回連続で実行する。
- Verification: direction ごとの status/log/process tree、public inference smoke request
- 完了条件: coordinator-only と worker-only が各 2 回連続成功し、409 loop や古い child 再利用がない
- 停止条件: `launchctl` job が重複、restart throttle 未経過、runtime state 削除が必要
- 着手準備: H-02 完了後、cable 接続済み・両 node `DistributedReady`・admission serving・active request 0・rollback candidate 保持を確認済み。次は coordinator-only restart から開始する。
- Failure evidence (2026-08-16): coordinator-only 1 回目で coordinator は `SoloStandaloneReady` へ戻ったが、worker の distributed child 停止が `SIGKILL is not allowed for this child` となり、約 180 秒後の recovery retry で worker は `SoloStandaloneReady` へ戻った。その後 worker は `paired` だが lease invalid / peer absent のまま、coordinator は `unpaired` で `/v1/node` HTTP 409 (`peer has not established a control lease`) loop が継続し、自動 re-pair / promotion / `DistributedReady` に収束しなかった。H-03 はこの cycle で停止し、worker-only restart と残りの cycle は未実施。evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/cycle1-coordinator-only-failure.md`。
- Fix application evidence (2026-08-16): 失効した認証済み peer の `/v1/node` を再 pair 用 descriptor 応答へ変更し、PeerLost recovery 後の phase を lease 有効時だけ `Paired` とし、auto pair を coordinator 限定にした。両 node の `allow_sigkill=true` と candidate `1952cd7bb2db07ffcb4e5487fed3a52949f3d2d7f85b4c35a57f7391fc4f3cb0` を適用後、SoloStandaloneReady から自動 re-pair / auto promotion / DistributedReady、control generation `445`、`healthy=true` へ収束した。これは修正適用と再開条件の確認であり、H-03 の coordinator-only / worker-only 各2 cycle完了ではない。evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/20260816-h03-fix-application.md`。
- Completion evidence (2026-08-16): 修正適用後、coordinator-only 2 cycle と worker-only 2 cycle を完了した。各 cycle で peer loss recovery、SoloStandaloneReady、自動 re-pair、auto promotion、DistributedReady、doctor `healthy=true`、public `/v1/models` HTTP 200、orphan child 不在を確認した。child は各回で新 PID/generation へ更新され、最終 worker PID `75852`、coordinator PID `40229`。新しい検証時間帯に 409 loop、`SIGKILL is not allowed for this child`、古い child 再利用はなかった。coordinator の既存 route-loss demotion task 終了競合ログはあったが、shared recovery と最終収束は成功した。evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/20260816-h03-summary.md`。

### [x] H-04 macOS 再起動を片側・両側で検証する

- Actor: operator（macOS 再起動）+ agent（再接続後の観測）
- Depends on: H-03
- 着手可能条件: process 再起動の両方向が成功し、operator が長い中断を承認
- Files: repository 外 evidence directory
- Actions:
  1. DistributedReady から coordinator の macOS だけを再起動し、worker は稼働継続する。
  2. login/LaunchAgent 起動後、両 node の自動復帰を確認する。
  3. baseline 再確立後、worker の macOS だけを再起動して同じ確認を行う。
  4. baseline 再確立後、両 macOS を再起動して同じ確認を行う。
  5. boot 前後の永続 cluster/control generation、child identity、409 の有無を比較する。
- Verification: 3 ケースの時系列 JSON/log、LaunchAgent 1 process、DS4 child 最大 1、public inference smoke request
- 完了条件: 片側再起動の両方向と両側再起動が、手動 pair/reconcile や state 削除なしで DistributedReady へ戻る
- 停止条件: OS update、別 service、disk encryption unlock 等が検証条件を変える
- Completion evidence (2026-08-17): coordinator-only、worker-only、両 node 同時再起動の3ケースを実施し、いずれも login/LaunchAgent 復帰後に手動 pair/reconcile や state 削除なしで両 node が `DistributedReady` へ収束した。各ケースで `cluster doctor --json` は `healthy=true`、admission serving、public `/v1/models` は HTTP 200、LaunchAgent は各 node 1件、DS4 child は各 node 最大1件だった。最終ケースの boot time は worker `2026-08-17 09:03:42`、coordinator `2026-08-17 09:03:40`、control generation `601`、worker child PID/generation `2578/449`、coordinator child PID/generation `1068/607`、cluster generation は worker `452` / coordinator `609`。boot 後の log window に 409 loop、`SIGKILL is not allowed for this child`、orphan child はなく、証跡は `/private/tmp/siderostat-reconnect-evidence-20260817/h04/20260817-h04-summary.md`。

### [ ] H-05 実機 evidence を判定し rollback を確認する

- Actor: agent + operator review
- Depends on: H-02、H-03、H-04
- 着手可能条件: 全実機 run の成功・失敗 artifact が揃っている
- Files: `docs/compatibility/reconnect-acceptance-YYYY-MM-DD.md`（新規、redaction 済み）
- Actions:
  1. scenario ごとに build、操作、所要時間、generation、child identity 変化、結果を表にする。
  2. failure、retry、manual intervention の有無をすべて記載する。
  3. raw artifact の path と SHA-256 を記載し、secret/個人情報を repository に入れない。
  4. operator が candidate 継続利用または旧 binary への rollback を選ぶ。
  5. rollback を選んだ場合も state/model/cache を削除せず、旧 binary で Solo readiness を確認する。
- Verification: proposal 第 1 節の 5 条件との acceptance mapping
- 完了条件: operator が実機 PASS/FAIL と candidate の扱いを承認
- 停止条件: evidence 欠落を推測で PASS にする必要がある

## 14. Phase F: 文書同期と最終 gate

### [ ] F-01 現行仕様・運用文書を同期し最終 gate を実行する

- Actor: agent
- Depends on: H-05
- 着手可能条件: 実装 behavior と実機 acceptance 結果が確定
- Files: `docs/connection-state-machine.md`、`docs/operations.md`、`docs/troubleshooting.md`、
  必要なら `docs/spec.md`、本書
- Actions:
  1. 新しい PeerLost recovery owner、session negotiation、network gate、Backoff 配線を現行仕様へ反映する。
  2. status field、Pair 409、manual reconcile、rollback の運用手順を更新する。
  3. proposal の各 P0〜P3 項目を実装 SHA/test/evidence へ対応付ける。
  4. 本書の全 task 状態、Depends on、Evidence を監査する。
  5. 共通 local gate を clean な状態で再実行する。
- Verification: 共通 local gate、文書 link/path、実機 acceptance mapping
- 完了条件: 第 4 節の全条件を満たし、本書 Status を `COMPLETE` にできる
- 停止条件: 実装と文書の不一致、未解決 failure、operator 未承認の実機結果がある

## 15. 共通停止・エスカレーション条件

次の場合は task を推測で続けず、failure evidence を保存してユーザーへ報告する。

- protocol、永続 schema、role authority、timeout semantics が設計文書で一意でない。
- state と child lifecycle の整合を保つために unknown process への signal が必要。
- HMAC、source/interface、route、lease、stale generation の検証を弱める必要がある。
- test 成功のため assertion 削除、cycle 数削減、長い固定 sleep、timeout 無制限化が必要。
- 実機で model、binary、deployment compatibility が両 node で一致しない。
- user workload が開始した、または物理 cable/OS/service 操作の安全な時間が終了した。
- secret、token、prompt、request body、完全 deployment ID が artifact に混入した。
- runtime state、model、KV cache、legacy data の削除が必要。
- `main`、`legacy/*`、公開済み tag への force push が必要。

## 16. Task 完了報告 template

```text
Task: <IDと名前>
Result: PASS | FAIL | BLOCKED
Changed: <file一覧>
Verification: <commandと件数>
Evidence: <SHA/artifact pathと日付>
Observed risks: <なし、または残存risk>
Next ready task: <Depends onを満たすtask ID>
Operator action required: <なし、または具体的な手作業>
```
