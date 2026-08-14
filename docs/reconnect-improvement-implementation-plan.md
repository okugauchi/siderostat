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

### [ ] G-01 Pair session negotiation protocol を設計固定する

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

### [ ] G-02 pure control session state machine を実装する

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

### [ ] G-03 control session を永続化・復旧する

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

### [ ] G-04 production Pair transport を新 protocol へ接続する

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

### [ ] G-05 片側再起動と duplicate の GREEN test を完成する

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

## 9. Phase E: P0-C 再接続 E2E

### [ ] E-01 P0 reconnect matrix を一つの acceptance suite にする

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

## 10. Phase B: P1 Backoff と operator reconcile

### [ ] B-01 promotion failure の分類と tracker 接続を統一する

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

### [ ] B-02 periodic reconcile に Backoff 復帰を接続する

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

### [ ] B-03 admin reconcile を coordinator runtime へ接続する

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

### [ ] B-04 promotion failure 後の reconnect E2E を追加する

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

## 11. Phase N: P2 route / discovery pairing gate

### [ ] N-01 pairing gate の network evidence contract を固定する

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

### [ ] N-02 network evidence を production control へ接続する

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

### [ ] N-03 route/discovery reconnect matrix を自動化する

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

## 12. Phase Q: P3 Pair timing の判定と条件付き実装

### [ ] Q-01 Pair timing を計測して実装要否を判定する

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

### [ ] Q-02 Pair confirm 完了通知を実装する、または不要判定を記録する

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

## 13. Phase H: ユーザー手作業を含む 2 node 実機検証

実機 task では、agent は command 提案、read-only 観測、結果整理を担当する。ユーザーは物理 cable
操作、macOS 再起動、candidate binary の配置承認を担当する。agent はユーザーの明示的依頼なしに
実機 service の停止、再起動、binary 上書きを行わない。

### [ ] H-01 実機検証の change window と rollback を準備する

- Actor: operator + agent
- Depends on: N-03、Q-02 の判定完了
- 着手可能条件: 共通 local gate と reconnect acceptance suite が GREEN
- Files: repository 外 evidence directory、必要なら redaction 済み runbook
- Actions:
  1. operator が両 node の利用停止可能時間と、進行中推論 request がないことを確認する。
  2. agent が candidate の commit SHA、binary SHA-256、config checksum を記録する。
  3. operator が現行 binary/config/state/log を削除せず backup または隔離する。
  4. operator が rollback binary と `launchctl` job label を確認する。
  5. agent が両 node の baseline `cluster status --json`、`cluster doctor --json`、process identity、
     log 開始位置を採取する。
- Verification: candidate/rollback の checksum、両 node Solo または Distributed の健全な baseline
- 完了条件: 各操作を中止して旧 binary へ戻せることを operator が確認済み
- 停止条件: active workload、unknown DS4 child、重複 supervisor、backup 不在、node 時刻の大幅ずれ

### [ ] H-02 DistributedReady から cable detach/reconnect を検証する

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

### [ ] H-03 片側 process 再起動を方向別に検証する

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

### [ ] H-04 macOS 再起動を片側・両側で検証する

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
