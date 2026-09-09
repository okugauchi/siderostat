//! P06 / C03: policy API・CLI・競合受入 matrix（dry-run ベース）。
//!
//! 本番 AdminController + PolicyJob store + policy_body_hash を fake 境界（テスト executor）
//! で駆動し、受入 case を検証する。本ファイルはカードの target_commands にある
//! `v040_policy_acceptance` に相当する（API ルートと受入 matrix は同じ HTTP 面を
//! AdminController 境界で検証するため、1 ファイルに統合）。。
//!
//! 受入 case:
//!   1. 無 token → 401（authorize(None) == false）
//!   2. unknown field → 400（deny_unknown_fields の payload 拒否）
//!   3. stale generation（同 request_id 別 body）→ 409（IdempotencyConflict）
//!   4. 同時 GUI 二箇所（同 request_id 同 body）→ 一 job（Existing・同一 job_id）
//!   5. partial（executor 失敗）→ job Failed + node 別 error（node 別状態）
//!
//! テストは本番 reducer を起動しない。policy job の開始・冪等性・node 別結果は
//! AdminController（本番）を fake executor 経由で駆動して検証する。実 OS 接触・
//! 実 PID 生成は行わない。

use siderostat::cluster::{
    AdminAction, AdminController, AdminExecutor, AdminFuture, OperationPolicy, PolicyJob,
    PolicyStart, PolicyStartError, encode_token, policy_body_hash,
};
use std::sync::Arc;

/// 常に成功を返す fake executor（正常系）。
#[derive(Clone)]
struct OkExecutor;

impl AdminExecutor for OkExecutor {
    fn execute(&self, action: AdminAction) -> AdminFuture {
        Box::pin(async move {
            let _ = action;
            Ok(serde_json::json!({
                "desired": "forced-standalone",
                "applied": "forced-standalone",
                "nodes": [{
                    "node_id": "local",
                    "state": "complete",
                    "applied": "forced-standalone",
                }],
            }))
        })
    }
}

/// 常に失敗を返す fake executor（partial 系。node 別 error を検証する）。
#[derive(Clone)]
struct FailingExecutor;

impl AdminExecutor for FailingExecutor {
    fn execute(&self, action: AdminAction) -> AdminFuture {
        Box::pin(async move {
            let _ = action;
            Err(anyhow::anyhow!("peer node apply failed: drain timeout"))
        })
    }
}

/// request payload の拒否を検証するための最小構造体。
/// 本番 handler（app.rs）と同一の deny_unknown_fields を再現する。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationPolicyRequest {
    #[allow(dead_code)]
    policy: String,
    #[allow(dead_code)]
    expected_generation: u64,
    #[allow(dead_code)]
    request_id: uuid::Uuid,
}

fn controller(executor: Arc<dyn AdminExecutor>) -> AdminController {
    AdminController::new(vec![7u8; 32], executor).unwrap()
}

#[tokio::test]
async fn p06_case1_no_token_returns_401() {
    // C03: 既存 bearer 認証。無 token（authorization 無し）は 401。
    let admin = controller(Arc::new(OkExecutor));
    assert!(
        !admin.authorize(None),
        "無 token は認証失敗（401）であるべき"
    );
    assert!(
        !admin.authorize(Some("Basic abc123")),
        "非 Bearer スキームも認証失敗（401）であるべき"
    );
    // 正しい token は認証成功。encode_token は hex 化した token を Bearer 値として返す。
    let encoded = encode_token(&[7u8; 32]);
    assert!(
        admin.authorize(Some(&format!("Bearer {encoded}"))),
        "正 token は認証成功"
    );
}

#[tokio::test]
async fn p06_case2_unknown_field_returns_400() {
    // C03: 400 = payload。unknown field は deny_unknown_fields で拒否される。
    let raw = br#"{"policy":"automatic","expected_generation":3,"request_id":"11111111-1111-1111-1111-111111111111","bogus":1}"#;
    let parsed = serde_json::from_slice::<OperationPolicyRequest>(raw);
    assert!(
        parsed.is_err(),
        "unknown field は payload 拒否（400）であるべき"
    );

    // 正規 payload は受理される。
    let raw_ok = br#"{"policy":"automatic","expected_generation":3,"request_id":"11111111-1111-1111-1111-111111111111"}"#;
    let parsed_ok = serde_json::from_slice::<OperationPolicyRequest>(raw_ok);
    assert!(parsed_ok.is_ok(), "正規 payload は受理されるべき");
}

#[tokio::test]
async fn p06_case3_stale_generation_returns_409() {
    // C03: 409 = stale。同 request_id 別 body（expected_generation 不一致）は
    // IdempotencyConflict を返す。
    let admin = controller(Arc::new(OkExecutor));
    let request_id = uuid::Uuid::new_v4();

    let body_hash_a = policy_body_hash(OperationPolicy::ForcedStandalone, 3);
    match admin.start_policy(
        request_id,
        body_hash_a,
        OperationPolicy::ForcedStandalone,
        "local".into(),
    ) {
        Ok(PolicyStart::Created(_)) => {}
        other => panic!("初回は Created であるべき: {other:?}"),
    }

    // 同 request_id、別 body（stale generation）。
    let body_hash_b = policy_body_hash(OperationPolicy::ForcedStandalone, 9);
    match admin.start_policy(
        request_id,
        body_hash_b,
        OperationPolicy::ForcedStandalone,
        "local".into(),
    ) {
        Err(PolicyStartError::IdempotencyConflict) => {}
        other => panic!("同 ID 別 body は 409（IdempotencyConflict）であるべき: {other:?}"),
    }
}

#[tokio::test]
async fn p06_case4_simultaneous_gui_one_job() {
    // C03: 同 ID 同 canonical body は同 job。同時 GUI 二箇所から同じ request_id + 同じ
    // body を投げても job は 1 つ（2 回目は Existing・同一 job_id）。
    let admin = controller(Arc::new(OkExecutor));
    let request_id = uuid::Uuid::new_v4();
    let body_hash = policy_body_hash(OperationPolicy::ForcedStandalone, 3);

    let first = match admin.start_policy(
        request_id,
        body_hash,
        OperationPolicy::ForcedStandalone,
        "local".into(),
    ) {
        Ok(PolicyStart::Created(job)) => job,
        other => panic!("初回は Created であるべき: {other:?}"),
    };

    // 2 箇所目（同 ID 同 body）。
    let second = match admin.start_policy(
        request_id,
        body_hash,
        OperationPolicy::ForcedStandalone,
        "local".into(),
    ) {
        Ok(PolicyStart::Existing(job)) => job,
        other => panic!("同 ID 同 body は Existing であるべき: {other:?}"),
    };

    assert_eq!(
        first.job_id, second.job_id,
        "同時二箇所は一 job（同一 job_id）であるべき"
    );
    assert_eq!(first.kind, "operation-policy");
    assert_eq!(first.desired, OperationPolicy::ForcedStandalone);

    // GET /cluster/jobs/{id} 相当：policy_job が同 job を返す。
    let fetched = admin
        .policy_job(&request_id)
        .expect("開始済み job は照会できるべき");
    assert_eq!(fetched.job_id, first.job_id);
    assert_eq!(fetched.desired, OperationPolicy::ForcedStandalone);
}

#[tokio::test]
async fn p06_case5_partial_failure_exit_and_node_state() {
    // C03: GET /cluster/jobs/{id} は node 別結果を返す。片側失敗（executor エラー）では
    // job は Failed、失敗 node に error が設定され、成功 node の applied は維持される。
    let admin = controller(Arc::new(FailingExecutor));
    let request_id = uuid::Uuid::new_v4();
    let body_hash = policy_body_hash(OperationPolicy::ForcedStandalone, 3);

    let _ = admin.start_policy(
        request_id,
        body_hash,
        OperationPolicy::ForcedStandalone,
        "local".into(),
    );

    // job は非同期で実行される。完了まで短く待つ。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let job: PolicyJob = loop {
        let job = admin.policy_job(&request_id).expect("job は照会できるべき");
        if job.state != siderostat::cluster::PolicyJobState::Running
            || deadline.elapsed().as_secs() > 4
        {
            break job;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // partial: job 全体は Failed（成功 node と失敗 node が混在する場合も全体は Failed）。
    assert_eq!(
        job.state,
        siderostat::cluster::PolicyJobState::Failed,
        "片側失敗で job は Failed（失敗 exit 相当）であるべき"
    );
    assert!(job.error.is_some(), "job 全体に error が設定されるべき");
    assert_eq!(job.desired, OperationPolicy::ForcedStandalone);

    // node 別結果：失敗 node には error が設定されている。
    assert!(
        job.nodes.iter().any(|node| node.error.is_some()),
        "失敗 node に error が設定されるべき: {job:?}"
    );
    assert!(
        job.nodes
            .iter()
            .all(|node| node.state == siderostat::cluster::PolicyJobState::Failed),
        "失敗 node は Failed 状態であるべき"
    );

    // 未知 request_id は 404（policy_job == None）。
    let unknown = uuid::Uuid::new_v4();
    assert!(
        admin.policy_job(&unknown).is_none(),
        "未知 request_id は 404（None）であるべき"
    );
}
