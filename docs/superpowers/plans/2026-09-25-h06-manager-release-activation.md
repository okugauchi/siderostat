# H06 Manager Release Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Execute directly without spawning subagents to keep token use predictable.

**Goal:** 各 node で DS4 の source・build・model を安全に準備し、検証済み profile の activate / rollback を runtime owner 経由で両 node に確実に反映する。job の成功と activation の完了は永続記録と実状態で裏付ける。

**Architecture:** `ManagerReleaseStore` を各 node の永続的な正本とし、既存 `ManagerExecutor` は Fetch から Stage までの domain job を実行する。Activate / Rollback は `ProductionClusterRuntime` が所有する非同期 coordinator actor へ typed request として渡す。actor は既存 supervisor に限定された command 差し替え口を使い、authenticated peer control channel の node ごとの durable acknowledgement を照合して transaction を完了する。

**Tech Stack:** Rust 2024、Tokio、Axum、serde JSON、既存 `ManagerExecutor` / `JobJournal`、Manager domain modules、`ProductionClusterRuntime`、既存 supervisor と authenticated peer control protocol、AppKit monitor GUI。

**Spec:** `docs/superpowers/specs/2026-09-25-h06-manager-release-activation-design.md`

## Global Constraints

- 承認済み仕様とユーザーの選択を維持する: 全 pipeline を対象とし、runtime-owned actor を使い、node ごとの GUI から node-local preparation を行う。
- peer 間で送るのは operation/profile/artifact ID、digest、generation、policy epoch、acknowledgement のみ。artifact 本体・パス・URL・command line は送らない。
- 公開 Manager API は runtime lease を受け取らない。actor が実行時点の lease、generation、policy epoch、node identity を取得・検証する。
- catalog は署名対象アプリ bundle 内の固定 resource だけを使う。GUI/API から任意 URL、path、shell command、catalog entry を登録できない。
- artifact、previous release、外部 baseline、model、既存 user configuration は削除・上書きしない。自動 cleanup と Siderostat self-update を追加しない。
- 管理対象外の path、symlink escape、未知 schema、短い hash、stale state、片 node の ack 欠落は fail-closed にする。壊れた journal を空 registry として扱わない。
- Activate は operator の明示操作だけで開始し、両 node の準備・readiness・commit ack と最終 live-state 検証が揃うまで Complete / route reopen を返さない。
- Manager executor と HTTP handler は child を直接 signal / start / stop しない。実プロセス操作は既存 supervisor owner 内でだけ行う。
- commit 前に `CONTRIBUTING.md` を読み、既存ブランチ `feature/v040-integration` の運用規約に従う。作業差分以外は stage しない。
- 既存の未コミット H06 差分を作業開始前に確認・保持する。同じファイルを変更する task の commit では task 所有の hunks だけを stage し、commit 前に staged diff を確認する。
- 物理受入で runtime を停止する作業は、着手前に見積もりを提示する。30分を超える見込みなら、ユーザーから延長許可を得るまで停止を伴う作業を行わない。
- 各タスクは failing test を先に追加し、対象テストを実行して RED を確認してから実装する。タスクごとに指定範囲だけを commit し、他の作業中差分を混ぜない。

## Review Focus

- Store の rename/fsync 順序と node/schema/reference/path/hash 検証が crash 後も旧 record を信頼可能な状態に保つか — Task 1。
- restart 時の Running/Cancelling job が Interrupted となり、成功へ推測復元されないか — Task 2。
- Fetch の main ancestry proof と Build の exact commit / clean workspace が永続 receipt に結びつくか — Tasks 3–4。
- catalog の ID allowlist、redirect allowlist、size、full SHA が download 前後と activation 直前に検証されるか — Task 5。
- Stage が profile の型付き artifact ID と validated config fingerprint だけを保存し、arbitrary argv/path を取り込まないか — Task 6。
- 既存 manifest の設定意図と、live runtime が実際に実行する digest を GUI/API が混同しないか — Tasks 6–7、12。
- runtime actor の実行中に policy、recovery、planned restart、pairing/promotion が競合して child ownership を奪わないか — Tasks 8–10。
- 両 node の個別 ack が失われた場合に Complete や admission reopen を誤って返さず、再起動時に candidate child を先行起動しないか — Tasks 9–11。
- rollback 失敗時も route/admission が閉じたままになり、previous と candidate の記録・実体を保持するか — Tasks 9–11。
- API、peer protocol、logs、GUI に raw path、credential、bearer token、build output、lease が漏れないか — Tasks 1–12。

---

### Task 1: Versioned ManagerReleaseStore と immutable artifact publication

**Files:**
- Create: `src/manager/store.rs`
- Modify: `src/manager/mod.rs`
- Modify: `src/manager/registry.rs`
- Test: `src/manager/store.rs` module tests

**Interfaces:**
- `ManagerReleaseStore::open(root: ManagerRoot, node_id: impl Into<String>) -> Result<Self, StoreError>` を追加する。
- `ManagerStoreSnapshot` は `schema_version: u32`, `node_id: String`, `source_receipts: BTreeMap<String, SourceRecord>`, `artifacts: BTreeMap<String, PersistedArtifactRecord>`, `profiles: BTreeMap<String, StagedProfileRecord>`, `release_pointers: ReleasePointers`, `jobs`, `activation_journals` を持つ。大きな artifact bytes は含めない。
- `PersistedArtifactRecord` は generated `id`, `kind`, managed-relative `rel_path`, `sha256`, `size`, `validation_state`, `provenance` を持つ。`StagedProfileRecord` は `profile_id`, `node_role`, verified role artifact IDs, model artifact/catalog ID, validated config fingerprint, compatibility result, hardware readiness を持ち、raw argv/path を持たない。`ReleaseIdentity` は `ManagedProfile(profile_id)` または `ExternalBaseline(fingerprint)` とし、`ReleasePointers` は active/previous identity を分けて保持する。
- `ArtifactDraft` は `kind`, `expected_sha256`, `expected_size`, `provenance` のみを受け、ID と managed relative path は server が生成する。`record_source(record: SourceRecord) -> Result<String, StoreError>`、`publish_artifact(source: &Path, draft: ArtifactDraft) -> Result<PersistedArtifactRecord, StoreError>`、`record_profile(record: StagedProfileRecord) -> Result<String, StoreError>`、`set_release_pointers(active: ReleaseIdentity, previous: Option<ReleaseIdentity>) -> Result<(), StoreError>` を追加する。
- `snapshot(&self) -> &ManagerStoreSnapshot` を読み取り境界として公開する。`ArtifactRegistry` は snapshot の validation/query facade とし、別の永続正本を持たない。

- [ ] **Step 1: Store の失敗テストを書く**
  - temp root に初期 store を開いて source/artifact/profile/reference を保存し、close/reopen 後に同じ snapshot が読めることを確認する。
  - schema version 未知、別 node の store、32-byte 未満の SHA、`..` path、root 外 symlink、参照先のない active profile をそれぞれ拒否する。
  - artifact bytes と期待 SHA が違う場合、record は publish されず、前 snapshot は reopen 後も残ることを確認する。
  - artifact bytes の rename 後・metadata publish 前の crash fixture では unreferenced file のみとなり、trusted record を作らないことを確認する。
- [ ] **Step 2: RED を確認する**
  - `cargo test --lib manager::store --features test-support`
  - 期待: `store` と strict load / artifact publication が未実装で失敗する。
- [ ] **Step 3: versioned store を実装する**
  - index を private operations 配下へ一時 file → `sync_all` → atomic rename → directory sync の順に publish する。
  - artifact を private temporary path に書き、full SHA/size を確認してから content-addressed managed path へ rename し、最後に metadata record を publish する。
  - 読み込み時に schema、node identity、相対 path、canonical containment、symlink、digest、全 record reference を検証する。未知 schema や不正 record は `StoreError` として返し、初期化し直さない。
  - `src/manager/mod.rs` で module と公開型を export し、既存 `ArtifactRegistry` の validator を store 経由で利用する。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --lib manager::store --features test-support`
  - 期待: crash/reopen、invalid schema、digest/path escape の各テストが通る。
- [ ] **Step 5: Task 1 を commit する**
  - `git add src/manager/store.rs src/manager/mod.rs src/manager/registry.rs`
  - `git commit -m "Add durable manager release store"`

### Task 2: Durable job journal と startup interruption handling

**Files:**
- Modify: `src/manager/jobs.rs`
- Modify: `src/manager/store.rs`
- Modify: `src/manager/api.rs`
- Modify: `src/app.rs`
- Test: `tests/v040_manager_store.rs`
- Test: `tests/v040_manager_api.rs`

**Interfaces:**
- `JobPhase::Interrupted` を追加し、公開 DTO の phase string を `interrupted` にする。
- `JobPersistence: Send + Sync` に `load() -> Result<(Vec<ManagerJob>, u64), PersistenceError>` と `save(records: &[ManagerJob], next_id: u64) -> Result<(), PersistenceError>` を定義し、store adapter を実装する。`JobJournal::open(persistence: Arc<dyn JobPersistence>)` と永続成功する `set_progress(id, progress)` を追加する。
- enqueue/progress/cancel/terminal transition は候補状態を先に persistence へ書き、保存成功後にだけメモリ上の状態を確定する。保存失敗時は以前の状態を返し、成功を公開しない。
- `AppState` は listener/executor 起動前に `ManagerReleaseStore` と persisted jobs を開く。restart 時の Running/Cancelling は `Interrupted` にして durable write する。
- store と journal の同時更新は常に journal lock → store lock の順とし、逆順の nested lock を作らない。

- [ ] **Step 1: restart と persistence の失敗テストを書く**
  - job の enqueue、progress、cancel、success/failure をそれぞれ保存して reopen 後に一致させる。
  - reopen 時に Running/Cancelling の job が `Interrupted` になり、同じ ID の late success が成功状態へ戻せないことを確認する。
  - terminal write の失敗を注入し、caller が成功を受け取らず、job を誤って succeeded と公開しないことを確認する。
  - API の job DTO が `interrupted` を返し、secret / raw output を含まないことを確認する。
- [ ] **Step 2: RED を確認する**
  - `cargo test --test v040_manager_store --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 3: 永続 transition と起動順を実装する**
  - job mutation を store snapshot と同期し、状態変更と永続化の失敗を `ManagerJobError` に伝播させる。未永続の `get_mut` による mutation を production path からなくす。
  - interrupted job を running dedup key に復元せず、同じ要求の再実行を新しい job として受け付けられるようにする。
  - `AppState` が executor と admin listener を起動する前に store をロードする。unreadable journal では起動を失敗させ、空 journal に置き換えない。
  - API status/error mapping へ Interrupted と persistence failure を追加する。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --test v040_manager_store --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 5: Task 2 を commit する**
  - `git add src/manager/jobs.rs src/manager/store.rs src/manager/api.rs src/app.rs tests/v040_manager_store.rs tests/v040_manager_api.rs`
  - `git commit -m "Persist manager jobs and mark interrupted work"`

### Task 3: Pinned DS4 Fetch receipt の永続化

**Files:**
- Modify: `src/manager/source.rs`
- Modify: `src/manager/store.rs`
- Modify: `src/manager/executor.rs`
- Test: `tests/v040_manager_pipeline.rs`

**Interfaces:**
- Production resolver は UI の `payload_key` を fixed `OfficialRemote` + pinned `main` fetch key にだけ解決する。
- Fetch 成功は `SourceRecord { remote, full_commit, main_proof, fetched_at }` を `ManagerReleaseStore` に保存した後に job success を返す。`main_proof` が pin 済み main の ancestry proof を表す。
- source receipt ID は store snapshot と `/manager/inventory` に掲載し、job ID と domain result ID は混同しない。job status は job lifecycle のみに使う。

- [ ] **Step 1: local bare fixture の失敗テストを書く**
  - local fixture remote の main commit を Fetch し、full SHA と main ancestry proof が store に保存されることを確認する。
  - 同じ store を process 相当で reopen し、同じ receipt ID と commit を取得する。
  - main の祖先でない commit、remote identity の差、キャンセルを失敗させ、source receipt と active pointer を変えない。
- [ ] **Step 2: RED を確認する**
  - `cargo test --test v040_manager_pipeline fetch_ --features test-support`
- [ ] **Step 3: Fetch result を durable record に結ぶ**
  - `RuntimeManagerBackend` の Fetch resolver を固定 official remote/main に接続する。
  - `SourceRecord` を store へ保存し、その durable receipt ID を inventory へ投影する。persist failure 後は job success を返さない。
  - エラー表示を固定 redacted class に写像し、remote credential や raw Git output を journal/API/log に保存しない。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --test v040_manager_pipeline fetch_ --features test-support`
- [ ] **Step 5: Task 3 を commit する**
  - `git add src/manager/source.rs src/manager/store.rs src/manager/executor.rs tests/v040_manager_pipeline.rs`
  - `git commit -m "Persist pinned manager source receipts"`

### Task 4: Commit-pinned isolated Build と immutable output record

**Files:**
- Modify: `src/manager/build.rs`
- Modify: `src/manager/executor.rs`
- Modify: `src/manager/store.rs`
- Test: `tests/v040_manager_pipeline.rs`

**Interfaces:**
- Build request は persisted source receipt ID と allowlisted role のみを指定する。workspace、target、argv は production resolver が managed root と固定 allowlist から組み立てる。
- Build は job ごとの unique checkout で exact full commit と clean worktree を確認してから既存 `build_artifacts` を実行する。
- 成功時の `BuildRecord`、role、target、flags、toolchain、arch、help digest、artifact full digest/size/path は immutable store record となる。

- [ ] **Step 1: Build fixture の失敗テストを書く**
  - receipt commit から2つの独立 job workspace が作られ、双方の HEAD が full commit と一致し、worktree が clean な状態でのみ make に到達する。
  - source ID 不明、HEAD 差、dirty workspace、未許可 role/target、欠落 output、cancel の各ケースで artifact record を publish しない。
  - 成功時は binary を content-addressed path に publish し、`BuildRecord` と help digest が reopen 後も一致する。active/previous pointer は変えない。
- [ ] **Step 2: RED を確認する**
  - `cargo test --test v040_manager_pipeline build_ --features test-support`
- [ ] **Step 3: isolated checkout と result publication を実装する**
  - workspace 作成・cleanup を managed root に限定し、Git process と既存 owned process-group runner の cancellation/reap 契約を保つ。
  - Build 成功後に output を store の verified publication path へ移し、record 永続化が終わるまで executor success を返さない。
  - workspace cleanup 失敗は安全な固定 error として記録し、公開済み artifact や active state を削除しない。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --test v040_manager_pipeline build_ --features test-support`
- [ ] **Step 5: Task 4 を commit する**
  - `git add src/manager/build.rs src/manager/executor.rs src/manager/store.rs tests/v040_manager_pipeline.rs`
  - `git commit -m "Build manager artifacts from pinned source receipts"`

### Task 5: Bundled model catalog、Download、Verify の durable path

**Files:**
- Modify: `resources/ds4/catalog.json`
- Modify: `src/manager/catalog.rs`
- Modify: `src/manager/download.rs`
- Modify: `src/manager/verify.rs`
- Modify: `src/manager/executor.rs`
- Modify: `src/manager/store.rs`
- Modify: `src/app.rs` (fail startup if the bundled catalog cannot be loaded)
- Test: `tests/v040_manager_pipeline.rs`
- Test: `tests/v040_catalog.rs` (placeholder catalog entries remain unavailable)
- Test: `tests/v040_build.rs` (expect the existing explicit Canceled result)

**Interfaces:**
- `bundled_catalog() -> Result<Vec<ModelCatalogEntry>, CatalogError>` は署名対象 bundle resource `resources/ds4/catalog.json` だけを読む。
- Download/Verify 入力は `catalog_id` または managed `artifact_id` のみ。production path で resolver に任意 entry/URL を登録する関数を使用しない。
- Download 成功は catalog size/full SHA/redirect allowlist を満たす bytes と immutable model record を保存する。Verify は実 bytes を再 hash して durable `Verified` state にする。

- [ ] **Step 1: allowlist と再検証の失敗テストを書く**
  - bundled ID のみが解決し、外部 ID/URL を指定する job は network access 前に拒否される。
  - disallowed redirect、size overrun/underrun、short/wrong SHA、途中キャンセルは trusted model record を作らない。
  - Verify の後で file を差し替えると Verify または activation 前 rehash が拒否し、state を Verified に保たない。
  - 成功 Download/Verify は store reopen 後に ID、digest、catalog provenance を保持する。
- [ ] **Step 2: RED を確認する**
  - `cargo test --test v040_manager_pipeline model_ --features test-support`
- [ ] **Step 3: bundled catalog と durable publish を実装する**
  - resource を strict parse し、duplicate ID、欠落 license/size/hash、無許可 scheme/host、unsafe redirect を起動時に拒否する。
  - Download は bounded temporary write から atomic artifact publication へ接続し、Verify は catalog/build provenance と実 bytes を照合する。
  - 現行 resource に信頼できる実 checksum/source 情報がない項目は、推測値で有効化せず unavailable/reference として扱う。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --test v040_manager_pipeline model_ --features test-support`
- [ ] **Step 5: Task 5 を commit する**
  - `git add resources/ds4/catalog.json src/manager/catalog.rs src/manager/download.rs src/manager/verify.rs src/manager/executor.rs src/manager/store.rs src/app.rs tests/v040_catalog.rs tests/v040_manager_pipeline.rs tests/v040_build.rs`
  - `git commit -m "Persist allowlisted manager model artifacts"`

### Task 6: Durable Stage profile と sanitized inventory API

**Files:**
- Modify: `src/manager/stage.rs`
- Modify: `src/manager/api.rs`
- Modify: `src/manager/executor.rs`
- Modify: `src/app.rs`
- Modify: `src/manager/store.rs`
- Test: `tests/v040_manager_api.rs`
- Test: `tests/v040_manager_pipeline.rs`

**Interfaces:**
- `ManagerInventoryResponse` を追加し、source commit、role artifact ID/digest、model ID/digest、staged profile、node-local readiness、active/previous digest、activation phase を公開する。raw path、URL、argv、lease、secret は含めない。
- Admin router に bearer-authenticated `GET /manager/inventory` を追加する。
- persisted `StagedProfileRecord` は verified artifact IDs、catalog ID、compatibility/hardware state、validated runtime-config fingerprint を保持し、unmanaged absolute path や argv を保存しない。
- `/manager/status` の active digest は設定済 manifest ではなく、verified record に結び付いた live runtime observation からだけ埋める。

- [ ] **Step 1: inventory redaction と Stage の失敗テストを書く**
  - 未検証/欠落/role 不一致/model family 不一致/prefix digest 差/未確認 RAM の各 profile は activation-ready にならない。
  - Stage 成功 record は referenced artifact IDs と config fingerprint を持ち、任意 argv/path を含まない。reopen 後も同じ profile が解決される。
  - 認証なし inventory は既存 admin auth と同じ拒否となり、認証済み応答は raw path/URL/lease を含まず、設定 manifest だけから active digest を合成しない。
- [ ] **Step 2: RED を確認する**
  - `cargo test --test v040_manager_pipeline stage_ --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 3: Stage と inventory projection を実装する**
  - Stage resolver が store の artifact records を再検査し、profile record の durable publication 後にだけ job success を返す。
  - API DTO は typed store snapshot と runtime live observation を別々に投影する。存在しない live proof は `None`/pending として返す。
  - `src/app.rs` の handler/router と monitor client が使う JSON schema を同期し、error class を固定・redacted にする。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --test v040_manager_pipeline stage_ --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 5: Task 6 を commit する**
  - `git add src/manager/stage.rs src/manager/api.rs src/manager/executor.rs src/app.rs src/manager/store.rs tests/v040_manager_api.rs tests/v040_manager_pipeline.rs`
  - `git commit -m "Expose durable manager inventory and staged profiles"`

### Task 7: Per-node GUI preparation flow

**Files:**
- Modify: `monitor/src/client.rs`
- Modify: `monitor/src/manager_window.rs`
- Modify: `monitor/src/main.rs`
- Test: `monitor/tests/v040_manager_window_host.rs`
- Test: `monitor/tests/v040_model_view.rs`
- Test: `monitor/tests/v040_manager_api.rs`

**Interfaces:**
- `MetricsClient` に `fetch_manager_inventory()` を追加し、既存 status/job refresh と同じ bearer-authenticated admin client を使う。
- `ManagerViewModel` は `ManagerInventoryResponse` を受け、node-local Fetch/Build/Download/Verify/Stage 操作の enabled state と readiness reason を導出する。
- クリック操作は既存 `ManagerCommand` / worker channel を通す。各 node GUI が自 node の preparation job だけを submit する。

- [ ] **Step 1: view model / client の失敗テストを書く**
  - local artifact が不足する node はその node の pipeline action を提示し、peer の artifact ID を local artifact として扱わない。
  - 未検証 catalog/profile、hardware pending、pending job の各状態で Activate が disabled、理由が表示される。
  - inventory JSON に path や secret を含めず、API failure は failed event として redacted 表示する。
  - host close/reopen と refresh 後に jobs / profile / digest が store snapshot から再表示される。
- [ ] **Step 2: RED を確認する**
  - `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_window_host --features test-support`
  - `cargo test --manifest-path monitor/Cargo.toml --test v040_model_view --features test-support`
- [ ] **Step 3: inventory-driven preparation UI を実装する**
  - monitor client の inventory decoding と worker event を追加する。
  - GUI は各 node の source, build/model artifact, verification, staged profile, active/previous state を inventory から描画し、準備操作の job 送信と polling を既存非同期経路へ接続する。
  - 設定 manifest 表示には「設定値」、live observation には「稼働中 digest」を区別して表示する。
- [ ] **Step 4: GREEN を確認する**
  - 上記2 focused monitor tests と `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_api --features test-support`
- [ ] **Step 5: Task 7 を commit する**
  - `git add monitor/src/client.rs monitor/src/manager_window.rs monitor/src/main.rs monitor/tests/v040_manager_window_host.rs monitor/tests/v040_model_view.rs monitor/tests/v040_manager_api.rs`
  - `git commit -m "Drive manager preparation UI from local inventory"`

### Task 8: Supervisor の検証済み command slot と排他 gate

**Files:**
- Modify: `src/cluster/process.rs`
- Modify: `src/cluster/process/standalone.rs`
- Modify: `src/cluster/process/coordinator.rs`
- Modify: `src/cluster/process/worker.rs`
- Modify: `src/cluster/operation.rs`
- Modify: `src/cluster/production.rs`
- Modify: `src/cluster/runtime.rs`, `src/cluster/coordinator.rs`, `src/cluster/worker.rs`
- Modify: `src/cluster/control.rs`, `src/cluster/production/effects.rs`,
  `src/cluster/production/pairing.rs`, `src/cluster/production/reconcile.rs`,
  `src/cluster/production/recovery.rs`, `src/cluster/production/policy.rs`, `src/app.rs`
- Test: supervisor unit tests, `src/cluster/operation.rs`, and `src/cluster/production.rs` tests

**Interfaces:**
- 各 supervisor は startup 時の固定 `Ds4Command` を、lifecycle owner が保持する検証済み `current_command` と `previous_command` に置き換える。child start は現在セットされた command snapshot を読む。
- production runtime に activation 用の排他 lease/gate を追加し、policy update、recovery、planned restart、pairing/promotion と同時に取得できない contract を定義する。
- candidate command は Stage record と現在の validated config から runtime owner が生成する。config fingerprint と executable/model digest が一致しない場合は slot に設定しない。実行前にも managed path、full digest、role を再検証する。

- [ ] **Step 1: command slot と gate の失敗テストを書く**
  - supervisor の stopped state で validated candidate を設定後に start すると candidate command digest を使い、running child がある場合は差し替えを拒否する。
  - lifecycle gate 中の policy/recovery/planned restart/pair request は既定の競合規則に従い拒否または defer し、二つの lifecycle mutation が同時に child を操作しない。
  - config fingerprint、path containment、artifact digest が古い candidate command は start 前に拒否される。
- [ ] **Step 2: RED を確認する**
  - `cargo test --lib cluster::process --features test-support`
  - `cargo test --lib cluster::production --features test-support`
- [ ] **Step 3: supervisor command slot と gate を実装する**
  - standalone/coordinator/worker の各 supervisor に限定された `set_next_command` / snapshot 機能を加える。HTTP と Manager executor へ child handle を公開しない。
  - existing operation serialization と planned restart gate に Manager activation claim を統合する。Force/standalone latch を Manager から解除しない。
  - new command の生成時に path、config fingerprint、executable/model digest、role compatibility を再検査する。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --lib cluster::process --features test-support`
  - `cargo test --lib cluster::production --features test-support`
- [ ] **Step 5: Task 8 を commit する**
  - `git add src/cluster/process.rs src/cluster/process/standalone.rs src/cluster/process/coordinator.rs src/cluster/process/worker.rs src/cluster/operation.rs src/cluster/production.rs src/cluster/runtime.rs src/cluster/coordinator.rs src/cluster/worker.rs src/cluster/control.rs src/cluster/production/effects.rs src/cluster/production/pairing.rs src/cluster/production/reconcile.rs src/cluster/production/recovery.rs src/cluster/production/policy.rs src/app.rs`
  - `git commit -m "Add runtime-owned DS4 command slots"`

### Task 9: Runtime command channel、local coordinator、startup reconciliation

**Files:**
- Create: `src/cluster/production/manager.rs`
- Modify: `src/cluster/production.rs`
- Modify: `src/cluster/production/activation.rs`
- Modify: `src/cluster/process/standalone.rs`
- Modify: `src/app.rs`
- Modify: `src/manager/api.rs`
- Modify: `src/manager/executor.rs`
- Modify: `src/manager/activation.rs`
- Modify: `src/manager/rollback.rs`
- Test: production manager coordinator tests
- Test: `tests/v040_manager_api.rs`

**Interfaces:**
- Add `ManagerRuntimeHandle` and typed `ManagerRuntimeCommand::{Activate, Rollback, Snapshot}` using Tokio `mpsc` and per-command `oneshot` replies. Synchronous executor calls use a blocking bridge only from the executor's `spawn_blocking` worker.
- `AppState` creates the bridge before admin listener startup; cluster-enabled nodes attach the receiver/coordinator to `ProductionClusterRuntime`, while cluster-disabled single-node mode attaches the same actor contract to the `StandaloneSupervisor` lifecycle owner. Missing runtime owner returns unavailable, never fixture success.
- Remove `runtime_lease` from public `JobSubmitRequest` and `ManagerExecutionRequest`; `expected_generation` remains required. Coordinator reads current generation, lease, role, peer identity, and policy epoch internally at execution time.
- Runtime startup calls `recover_manager_activation_journals()` before any DS4 supervisor starts; unresolved recovery closes admission and prevents candidate child start.

- [ ] **Step 1: actor-boundary and crash-recovery testsを書く**
  - public Activate/Rollback API with runtime lease field is rejected as unknown JSON field; with a nonzero expected generation it reaches the actor without caller-provided lease.
  - actor rejects stale generation, unavailable/expired lease, stale policy epoch, unpaired cluster peer, and conflicting lifecycle operation before drain.
  - local single-node activation persists intent/phases, stops old through supervisor owner, starts candidate, waits real readiness, then persists active/previous pointers and success.
  - startup cases at Intent, Draining, Starting, Ready, Committing, and Rollback recover before child start; unprovable state leaves admission closed and requires manual intervention.
- [ ] **Step 2: RED を確認する**
  - `cargo test --lib cluster::production::manager --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 3: runtime-owned local coordinator を実装する**
  - coordinator は immutable operation ID と expected generation を受け、store の verified profile を再hashし、runtime-owned lease/policy/gate を検証してから durable intent を書く。
  - `activation.rs` の test-only assumption「1 driver commit が両 node ack」を削り、local participant の明示 readiness/commit ack を要求する state transitions に改める。
  - command channel を `AppState` と cluster runtime / `StandaloneSupervisor` の lifecycle owner に接続する。standalone は同じ1 participant protocol を通す。
  - startup path で Manager activation journal recovery を DS4 child start より先に実行する。ambiguous transaction では admission を閉じ、manual recovery state を公開する。
- [ ] **Step 4: GREEN を確認する**
  - `cargo test --lib cluster::production::manager --features test-support`
  - `cargo test --test v040_manager_api --features test-support`
- [ ] **Step 5: Task 9 を commit する**
  - `git add src/cluster/production/manager.rs src/cluster/production.rs src/cluster/production/activation.rs src/cluster/process/standalone.rs src/app.rs src/manager/api.rs src/manager/executor.rs src/manager/activation.rs src/manager/rollback.rs tests/v040_manager_api.rs`
  - `git commit -m "Add runtime-owned manager activation coordinator"`

### Task 10: Authenticated per-node Manager participant protocol

**Files:**
- Modify: `src/cluster/production.rs`
- Modify: `src/cluster/production/manager.rs`
- Modify: `src/cluster/production/activation.rs`
- Test: `src/cluster/production.rs` peer protocol tests
- Test: `src/cluster/production/manager.rs` participant tests

**Interfaces:**
- peer DTO は `operation_id`, node-local `profile_id`, candidate/previous full digest, expected generation, policy epoch, phase、ack ID のみを含む。raw paths、binary bytes、runtime lease、secret は含めない。
- coordinator に `/v1/manager/prepare`, `/v1/manager/drain`, `/v1/manager/start`, `/v1/manager/commit`, `/v1/manager/rollback`, `/v1/manager/status` の authenticated control routes/client methods を追加する。実際の prefix/version は既存 peer compatibility policy と照合し、未対応 peer は drain 前に拒否する。
- 各 peer participant は operation ID ごとの durable journal と idempotent phase transition を持ち、ローカル supervisor を介してのみ child lifecycle を操作する。

- [ ] **Step 1: protocol contract の失敗テストを書く**
  - unauthenticated、wrong role、unknown field、stale operation ID、digest/profile mismatch の要求が child/lifecycle state を変更せず拒否される。
  - duplicate prepare/drain/start/commit は同じ durable ack を返し、異なる payload の同一 operation ID は拒否される。
  - peer request/response serialized bytes に artifact bytes、absolute paths、URLs、runtime lease、bearer/HMAC secret が存在しない。
  - unreachable/old-version peer は coordinator preflight で止まり、local child drain 回数が0のままになる。
- [ ] **Step 2: RED を確認する**
  - `cargo test --lib --features test-support manager_peer_`
  - `cargo test --lib --features test-support cluster::production::manager`
- [ ] **Step 3: authenticated participant protocol を実装する**
  - 既存 `ControlAuthenticator` / replay protections / role checks を再利用し、peer client と control router へ typed endpoints を追加する。
  - worker は自 node の staged profile ID を local store から引き、受信digestと一致する場合だけ prepare/start/commit を実行する。
  - phase transition ごとに local journal を永続化してから response ack を返す。ack が失われても再送が安全な operation ID semantics を保つ。
- [ ] **Step 4: GREEN を確認する**
  - 上記 focused peer protocol tests。
- [ ] **Step 5: Task 10 を commit する**
  - `git add src/cluster/production.rs src/cluster/production/manager.rs src/cluster/production/activation.rs`
  - `git commit -m "Add authenticated manager peer participants"`

### Task 11: Two-node commit、rollback、phase reconciliation

**Files:**
- Modify: `src/cluster/production/manager.rs`
- Modify: `src/cluster/production/activation.rs`
- Modify: `src/cluster/production/reconcile.rs`
- Modify: `src/manager/activation.rs`
- Modify: `src/manager/rollback.rs`
- Test: `src/cluster/production/manager.rs` transaction/fault tests

**Interfaces:**
- coordinator actor が global operation journal を持ち、両 local participant から同 operation/profile compatibility/digest に対する durable prepare、drain、ready、commit ack を個別に記録する。
- Global transaction phase は `Preparing`, `Draining`, `Starting`, `Committing`, `RollingBack`, `Complete`, `ManualIntervention` のいずれか。`Complete` と route reopen は両 commit ack + 最終 generation/policy/lease check 後だけ行う。
- Rollback は前 active を両 node で restore/readiness 確認する。片 node failure、lost ack、peer disconnect、ambiguous recovery は ManualIntervention/admission closed とする。

- [ ] **Step 1: fault matrix の失敗テストを書く**
  - 片 node artifact missing、peer disconnect、local/remote drain failure、candidate start failure、片側 ready/commit ack loss、Force policy change、stale lease/generation、duplicate activation を注入する。
  - 片側 ack だけでは Complete にならず、route closed のまま保持されることを確認する。
  - candidate start/commit の失敗で previous へ両 node rollback し、成功時のみ previous pointer が前 active、active pointer が戻した release となる。
  - previous start failure または再起動後に状態を証明できない場合は child candidate を開始せず、ManualIntervention + admission closed を維持する。
  - crash point ごとの再送で operation ID が二重 child start/二重 commit を起こさない。
- [ ] **Step 2: RED を確認する**
  - `cargo test --lib cluster::production::manager --features test-support`
  - `cargo test --lib cluster::production::activation --features test-support`
- [ ] **Step 3: transaction orchestration と reconciler を実装する**
  - preflight → durable prepare → both drain → both candidate start/readiness → per-node commit → final live check の順で phase を永続化する。
  - どの失敗経路でも既存 Force latch/latest policy epoch を維持し、recoverable failure は previous restore、証明不能は admission closed にする。
  - current live process identity と store pointer を照合してから runtime inventory に active digest を公開する。
  - `activation::execute_activation` の fake driver-only completion を削除し、test doubles でも local/peer ack を別個に返させる。
- [ ] **Step 4: GREEN を確認する**
  - transaction/fault test filters と `cargo test --lib cluster::production::activation --features test-support`
- [ ] **Step 5: Task 11 を commit する**
  - `git add src/cluster/production/manager.rs src/cluster/production/activation.rs src/cluster/production/reconcile.rs src/manager/activation.rs src/manager/rollback.rs`
  - `git commit -m "Reconcile two-node manager activation transactions"`

### Task 12: Manager GUI transaction view と final automated gates

**Files:**
- Modify: `monitor/src/client.rs`
- Modify: `monitor/src/manager_window.rs`
- Modify: `monitor/src/main.rs`
- Modify: `monitor/tests/v040_manager_window_host.rs`
- Modify: `monitor/tests/v040_model_view.rs`
- Modify: `monitor/tests/v040_manager_api.rs`

**Interfaces:**
- GUI inventory は node ごとの prepare/readiness/active/previous digest と global activation phase / redacted failure reason を表示する。
- Activate は双方の local readiness、profile compatibility、generation、policy の公開済み状態が揃うまで disabled。Rollback は previous release が実在・検証済みの場合のみ明示操作可能。
- job terminal / activation Complete は API status と live inventory refresh で確認してから GUI に反映する。physical evidence が揃うまで H06 は `PENDING` のまま。

- [ ] **Step 1: end-to-end GUI gate の失敗テストを書く**
  - peer readiness が欠落/古い/不一致なら activation control が disabled で、理由が redacted 表示される。
  - 正常時に operator が Activate/rollback を明示 submit し、GUI は per-node phase と live digest を refresh 後にだけ更新する。
  - one-sided commit、manual intervention、interrupted job、rollback failure は Complete/Ready と描画されない。
- [ ] **Step 2: RED を確認する**
  - `cargo test --manifest-path monitor/Cargo.toml --test v040_manager_window_host --features test-support`
  - `cargo test --manifest-path monitor/Cargo.toml --test v040_model_view --features test-support`
- [ ] **Step 3: transaction UI と最終受入導線を実装する**
  - inventory refresh と typed Activate/Rollback submission を既存 worker channel に接続し、AppKit main thread で network/process I/O を行わない。
  - per-node phase、active/previous digest、固定 failure class、operator action を表示し、設定 manifest と live observation を明確に分ける。
  - `siderostat-review` を適用し、実装レビューの指摘を直してから仕様の fault matrix を automated test に対応付ける。
- [ ] **Step 4: 全自動 gate を確認する**
  - `cargo test --all-targets --features test-support -- --skip w09_all_fixtures_accepted_by_real_codex_parser`
  - `cargo test --manifest-path monitor/Cargo.toml --all-targets --features test-support`
  - `cargo fmt --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo xtask app-dev --version 0.4.0 --build-number 3 --verify`
- [ ] **Step 5: Task 12 を commit する**
  - GUI の変更だけを stage し、全 suite/app bundle の成果物や runtime state を commit に混ぜない。
  - `git add monitor/src/client.rs monitor/src/manager_window.rs monitor/src/main.rs monitor/tests/v040_manager_window_host.rs monitor/tests/v040_model_view.rs monitor/tests/v040_manager_api.rs`
  - `git commit -m "Show two-node manager activation state in GUI"`

### Task 13: Two-node physical acceptance と証跡

**Files:**
- Modify: `docs/superpowers/specs/2026-09-25-h06-manager-release-activation-design.md` (Acceptance の実測記録)

**Interfaces:**
- 物理 acceptance 記録には node ごとの preparation 結果、operation ID、各 phase / ack、candidate と previous の live digest、停止時間、復旧結果だけを含める。secret、raw path、raw command、credential、build log は記録しない。
- 実機 acceptance の成否は GUI/runtime の観測に基づき、automated fixture の成功から推定しない。

- [ ] **Step 1: 停止時間を見積もる**
  - 各 node の artifact preparation は停止不要の事前作業として完了させ、candidate activation、failure rollback、restart reconciliation、explicit rollback に必要な runtime 停止時間を着手前に提示する。
  - 停止時間が30分以内なら既存許可範囲で続行する。30分を超える見込みならruntimeを停止する前にユーザーへ延長を求め、返答までは非停止の準備だけを進める。実行中に30分へ達した場合も、新しい破壊的段階へ進む前に状況と延長を求める。
- [ ] **Step 2: 両 GUI から transaction acceptance を行う**
  - 各 node で自身の Fetch/Build/Download/Verify/Stage を実行し、inventory の local readiness を確認する。
  - 明示 Activate 後、両 node の durable prepare/drain/start/readiness/commit ack と live active digest を確認する。
  - candidate start failure と transaction interruption/restart をそれぞれ起こし、previous への両 node restore または manual intervention + admission closed を確認する。最後に explicit rollback の両 node readiness を確認する。
- [ ] **Step 3: Acceptance 結果を記録する**
  - 実測結果を仕様書の Acceptance 記録欄へ追記する。未実施・失敗した項目を明示し、証明できない場合は H06 を `PENDING` のままにする。
  - `git diff --check` を実行し、秘密情報が入っていないことをレビューしてから対象仕様書だけを commit する。
- [ ] **Step 4: acceptance evidence を commit する**
  - `git add docs/superpowers/specs/2026-09-25-h06-manager-release-activation-design.md`
  - `git commit -m "Record H06 manager physical acceptance"`

## Completion Criteria

- Tasks 1–12 の automated acceptance が通り、project review skill の指摘が解消されている。
- `cargo xtask app-dev ... --verify` が成功し、実機操作を伴わない bundle 検証が終わっている。
- 両 node の GUI preparation、two-node activation、rollback、restart reconciliation を観測した記録が存在する。未実施なら H06 を PENDING のままにする。
- 最終報告は commit/push 状況、検証結果、実機受入の範囲と残件を分けて示す。push は新たな明示依頼がある場合だけ行う。
