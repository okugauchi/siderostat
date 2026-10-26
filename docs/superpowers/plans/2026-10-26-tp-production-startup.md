# TP通常起動経路 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 通常のserveからRDMA TPを起動・監視し、障害時に検証済Standaloneへ戻す。

**Architecture:** 既存ProductionClusterRuntimeにtopology別のdeploymentとTP lifecycleを接続する。LPのv1制御は維持し、TPは署名付きv2制御と既存reducerを使う。child操作は共通排他、準備確認はowned childの実観測と専用warm-upに限定する。

**Tech Stack:** Rust、Tokio、Axum、serde、reqwest。新規依存なし。

**Spec:** ../specs/2026-10-26-tp-production-startup-design.md（2026-10-26承認済み）

## Global Constraints

- モデル追加・モデル設定・Vision・DSpark・実機導入・パッケージ生成は対象外。
- 既定LP、schema2 identity、公開推論API、proxy timeout、recovery opt-in、release versionを維持する。
- TPはRDMAのみ、workerはds4、coordinatorはds4-server。旧DS4D HELLOを使わない。
- 実稼働サービス・secret・モデル・ネットワーク設定へ変更を加えない。
- testsはtemp root、ephemeral port、fake lifecycleを使う。

## Review Focus

- context/RDMA/roleが実spawnのargvへ反映されること（Task 1）。
- 旧sessionとchild交換後のログでReadyにならないこと（Task 2/3）。
- HTTP準備完了だけではwarm-upを省略できないこと（Task 3）。
- Forceと再起動・復旧のcross-kind競合で二重child操作が起きないこと（Task 4）。
- Standalone成功・自動再起動でretry budgetが復活しないこと（Task 4）。

## Task 1: deploymentと起動コマンド

**Files:** src/cluster/deployment.rs（新規）、src/cluster/ds4_command.rs、src/cluster/manifest.rs、src/config.rs、src/app.rs、xtask/src/manifest.rs、tests/tp_production_startup.rs（新規）。

**Interfaces:** RuntimeDeployment::{LayerParallel(DistributedManifest), TensorParallel(TpDeploymentManifest)}、parse(bytes, topology)、validate()、deployment_id()、model_family()。TP role artifact/capabilityを検証し、role別コマンドを解決する。

- [ ] context・RDMA引数・別role binary・layers無しをassertするテストを先に追加。
- [ ] `cargo test --test tp_production_startup --features test-support`で未接続の失敗を確認。
- [ ] schema2/3をtopologyで分岐し、既存LP呼び出しにはFrom adapterを提供。
- [ ] TPではschema3とcapability照合を使い、LP builderと固定model familyを使用しない。
- [ ] 対象テストと既存v040_config/v040_manifest相当を実行し、成功後commit。

## Task 2: 実child観測

**Files:** src/cluster/process/tp.rs（新規）、src/cluster/ds4_log.rs、src/cluster/worker.rs、src/cluster/platform/rdma.rs、tests/tp_production_startup.rs。

**Interfaces:** TpLocalLifecycle（start(session)、status(session)、stop、child_identity）、TpLocalStatus（running、connected、http_ready）。本番はManagedChild/SupervisedSlot、テストは注入adapter。

- [ ] source由来ログの正例・不完全行・旧session・child交換をテスト化してREDを確認。
- [ ] TP接続完了とRDMA transportの明示観測、coordinator HTTP準備、owned child生存を実装。
- [ ] RDMAは読取probeでmember/route/device/GIDを確認。設定変更やTP socket probeは禁止。
- [ ] 対象テストをGREENにしcommit。

## Task 3: 本番のTP起動・制御

**Files:** src/cluster/production/tp_runtime.rs、src/cluster/production/tp_control.rs（新規）、src/cluster/production.rs、src/cluster/production/pairing.rs、src/cluster/production/reconcile.rs、src/cluster/state.rs、src/proxy.rs。

**Interfaces:** version付きTP制御、session付きprepare/status/ready/stop。本番promoteはtopologyで分岐し、TPではworker Prepared→coordinator start→両child/handshake/HTTP→warm-up→Readyを実行する。

- [ ] 2nodeの実HTTP制御とfake lifecycleで順序・各準備欠落・旧peerをテストしREDを確認。
- [ ] v1を変更せずv2 TP機能を交渉。session/control generation/policy epochを個別照合。
- [ ] admissionを閉じdrain、worker先行で起動し、期限付き専用permitのwarm-up後に公開。
- [ ] dry-runも同じ制御/reducerで駆動。Ready直接投入のテスト専用promoteを除去。
- [ ] 対象テストとreconnect suiteをGREENにしcommit。

## Task 4: 排他・永続化・復帰

**Files:** src/cluster/operation.rs、src/cluster/state_store.rs、src/cluster/recovery_tp.rs、src/cluster/production/tp_runtime.rs、src/cluster/production/policy.rs、src/cluster/production/recovery.rs、src/app.rs、tests/tp_production_startup.rs。

**Interfaces:** cross-kind共通owner、TP session/retry journal、TP障害の単一復帰owner。初回に追加2回、Standalone成功と自動再起動でresetしない。

- [ ] cross-kind衝突、Force競合、同時障害、fallback失敗、budget再読込のREDを確認。
- [ ] 既存leaseを全kind共通ownerへ変更。Force intentは保存を先行しeffectを直列化。
- [ ] journalへsessionとincident/budgetを保存し、既存state snapshotとの上書き競合を防ぐ。
- [ ] 失敗時は受付閉→session無効→drain→owned child回収→Standalone確認の順で復帰。
- [ ] policy/auto_promote/auto_demoteを尊重。上限後の解除は承認済設計の明示操作だけ。
- [ ] 再接続10cycleと並列reconnect、workspace testsをGREENにしcommit。

## Task 5: 公開文書・全体検証・レビュー

**Files:** README.md、README.ja.md、docs/operations.md、docs/operations.ja.md、docs/troubleshooting.md、docs/troubleshooting.ja.md、TP設定例、設計/計画の進捗。

- [ ] TP設定・準備不足時の診断・停止復帰・ソフトウェア受入と実機受入の差を記載。
- [ ] `cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-targets --all-features`を実行。
- [ ] `cargo test --test reconnect_production --features test-support -- --test-threads=8`と差分検査。
- [ ] 独立レビューとsiderostat-reviewの全観点を適用し、指摘を修正・再検証・再レビュー。
- [ ] 実機未検証を明記して結果を報告。push/merge/配布を行わない。
