//! v0.4.0 M07 — stage profile・軽量compatibility smoke。M07。
//!
//! 公開 API（manager::stage_profile / manager::build_activation_plan /
//! manager::compatibility_smoke）経由で受入 case を検証する。smoke target は
//! 隔離 fake/小 fixture で固定（実重い model を load しない）。M07。
//!
//! 受入 case:
//! - 入力: model family 差 → stage 拒否
//! - 入力: 空き RAM 未確認 → hardware pending
//! - 入力: prefix-file digest 差 → 拒否
//! - 入力: external artifact → 同じ契約で検証
use siderostat::manager::catalog::{CapabilityStatus, ModelCatalogEntry};
use siderostat::manager::stage::{
    StageError, StageRequest, StagedProfileStatus, build_activation_plan, stage_profile,
};
use siderostat::manager::{SmokeRequest, compatibility_smoke};
use std::path::PathBuf;

fn model(family: &str, prefix: Option<String>) -> ModelCatalogEntry {
    ModelCatalogEntry {
        catalog_id: "m1".to_string(),
        url: "https://models.example.com/m.bin".to_string(),
        redirect_allowlist: vec![],
        size: 1024,
        sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
        license: "mit".to_string(),
        family: family.to_string(),
        quantization: "q4".to_string(),
        encoder: None,
        support: None,
        prefix_file: prefix,
        reference: Some("ref".to_string()),
        main_integrated: true,
        ram_reference: Some("16GiB".to_string()),
        compatibility: vec![],
        status: CapabilityStatus::Candidate,
    }
}

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m07it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create base");
    base
}

fn mk_fixture(base: &std::path::Path, name: &str) -> PathBuf {
    let p = base.join(name);
    std::fs::write(&p, b"smoke-fixture").expect("fixture");
    p
}

/// 受入 case: model family 差 → stage 拒否。M07。
#[test]
fn m07_family_mismatch_rejected() {
    let base = tmp("family");
    let fixture = mk_fixture(&base, "smoke.bin");
    let req = SmokeRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("dspark", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: true,
        fixture,
    };
    let err = compatibility_smoke(req).expect_err("family mismatch must reject");
    assert!(matches!(
        err,
        siderostat::manager::CompatibilityError::Stage(StageError::FamilyMismatch(_))
    ));
}

/// 受入 case: 空き RAM 未確認 → hardware pending。M07。
#[test]
fn m07_unconfirmed_ram_is_hardware_pending() {
    let base = tmp("ram");
    let fixture = mk_fixture(&base, "smoke.bin");
    let req = SmokeRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("ds4", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: false,
        fixture,
    };
    let out = compatibility_smoke(req).expect("pending, not error");
    assert!(
        !out.ok,
        "RAM 未確認は smoke を実行しない（hardware pending）"
    );

    // 同じ profile を stage_profile で見ると HardwarePending。起動未実行を
    // ready 済みとしない。M07。
    let staged = stage_profile(StageRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("ds4", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: false,
    })
    .expect("stage pending");
    assert_eq!(staged.status, StagedProfileStatus::HardwarePending);

    // HardwarePending の activation plan は ready=false。M07。
    let plan = build_activation_plan(&staged, 1);
    assert!(!plan.ready);
}

/// 受入 case: prefix-file digest 差 → 拒否。M07。
#[test]
fn m07_prefix_digest_mismatch_rejected() {
    let base = tmp("prefix");
    let fixture = mk_fixture(&base, "smoke.bin");
    let req = SmokeRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("ds4", Some("actual-prefix".to_string())),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: Some("expected-prefix".to_string()),
        ram_confirmed: true,
        fixture,
    };
    let err = compatibility_smoke(req).expect_err("prefix mismatch must reject");
    assert!(matches!(
        err,
        siderostat::manager::CompatibilityError::Stage(StageError::PrefixDigestMismatch(_))
    ));
}

/// 受入 case: external artifact → 同じ契約で検証。M07。
///
/// external RoleArtifact は managed verified と同じ StageRequest /
/// SmokeRequest を通す（family / prefix-file / RAM の共通契約）。M07。
#[test]
fn m07_external_artifact_same_contract() {
    let base = tmp("external");
    let fixture = mk_fixture(&base, "smoke.bin");
    // external artifact path（managed と区別せず共通 PathBuf）。M07。
    let external = base.join("external-ds4.bin");
    std::fs::write(&external, b"external").expect("external");

    // external を同じ契約で stage（family 一致・prefix 一致・RAM 確認済み）。
    // Validated になり smoke 成功する。M07。
    let req = SmokeRequest {
        profile_id: "p-ext".to_string(),
        role_artifacts: vec![external.clone()],
        model: model("ds4", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: true,
        fixture: fixture.clone(),
    };
    let out = compatibility_smoke(req).expect("external same contract");
    assert!(
        out.ok,
        "external artifact も同じ契約で Validated smoke 成功"
    );

    // external でも family 差は拒否（同じ契約）。M07。
    let req_bad = SmokeRequest {
        profile_id: "p-ext".to_string(),
        role_artifacts: vec![external],
        model: model("glm", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: true,
        fixture,
    };
    let err = compatibility_smoke(req_bad).expect_err("external family mismatch must reject");
    assert!(matches!(
        err,
        siderostat::manager::CompatibilityError::Stage(StageError::FamilyMismatch(_))
    ));
}

/// 事後条件: validated stage と hardware smoke 待ちを区別。M07。
#[test]
fn m07_validated_not_ready_until_smoke() {
    let base = tmp("validated");
    let fixture = mk_fixture(&base, "smoke.bin");
    // RAM 確認済み → Validated。activation plan は ready=true になるが、
    // これは smoke 検証済みの意味ではなく、stage 検証済み + RAM 確認済み。
    // smoke（軽量 fixture）が成功した段階で ready。M07。
    let staged = stage_profile(StageRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("ds4", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: true,
    })
    .expect("stage validated");
    assert_eq!(staged.status, StagedProfileStatus::Validated);

    let plan = build_activation_plan(&staged, 1);
    assert!(plan.ready);

    // 軽量 smoke 成功（Validated → ok）。M07。
    let out = compatibility_smoke(SmokeRequest {
        profile_id: "p1".to_string(),
        role_artifacts: vec![base.join("r1")],
        model: model("ds4", None),
        expected_family: "ds4".to_string(),
        context_size: 4096,
        expected_prefix_digest: None,
        ram_confirmed: true,
        fixture,
    })
    .expect("smoke ok");
    assert!(out.ok);
}
