//! M04 — commit 別 model catalog・組み合わせ。受入 matrix。M04。
//!
//! 本ファイルは M04 カードの `v040_catalog.rs` に対応する。公開 API
//! （`load_and_validate` / `validate_entry` / `compute_status` /
//! `CapabilityStatus` / `CatalogError` / `ModelCatalogEntry`）を介して受入
//! case を検証する。`resources/ds4/catalog.json` を fixture として読み込む
//! （実 model のダウンロードはしない）。M04。
//!
//! 受入 case（全て必須）:
//! - 入力: Vision + 0731 support → 拒否
//! - 入力: TP + 無根拠 DSpark → 拒否
//! - 入力: AProjQ4 未 main → reference
//! - 入力: 欠落 checksum → activation 不可
//!
//! レビュー重点: 性能再現を登録条件にしない。サイズ/RAM は upstream
//! reference と明示する。reference PR を stable にしない。M04。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M04。
use siderostat::manager::catalog::{
    CapabilityStatus, CatalogError, ModelCatalogEntry, compute_status, load_and_validate,
    validate_entry,
};
use std::path::Path;

fn catalog_path() -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    root.join("resources/ds4/catalog.json")
}

fn base_entry(id: &str) -> ModelCatalogEntry {
    ModelCatalogEntry {
        catalog_id: id.to_string(),
        url: format!("https://models.example.com/{id}.bin"),
        redirect_allowlist: vec!["https://models.example.com/".to_string()],
        size: 1024,
        sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
        license: "mit".to_string(),
        family: "ds4".to_string(),
        quantization: "q4".to_string(),
        encoder: None,
        support: None,
        prefix_file: None,
        reference: Some("https://github.com/example/ds4".to_string()),
        main_integrated: true,
        ram_reference: Some("16GiB (upstream reference)".to_string()),
        compatibility: vec![],
        status: CapabilityStatus::Reference,
    }
}

/// 受入 case 1: Vision + 0731 support → 拒否。M04。
#[test]
fn m04_vision_0731_rejected() {
    let mut entry = base_entry("vision-0731");
    entry.family = "vision".to_string();
    entry.support = Some("0731".to_string());
    let err = validate_entry(entry).expect_err("vision+0731 must be rejected");
    assert!(matches!(err, CatalogError::Unsupported(_)));
}

/// 受入 case 2: TP + 無根拠 DSpark → 拒否。M04。
#[test]
fn m04_tp_unfounded_dspark_rejected() {
    let mut entry = base_entry("dspark-tp-no-ref");
    entry.family = "dspark".to_string();
    entry.compatibility = vec!["tp".to_string()];
    entry.reference = None;
    let err = validate_entry(entry).expect_err("tp dspark without reference must be rejected");
    assert!(matches!(err, CatalogError::Unfounded(_)));
}

/// 受入 case 3: AProjQ4 未 main → reference。M04。
#[test]
fn m04_unmain_integrated_is_reference() {
    let mut entry = base_entry("a-proj-q4");
    entry.family = "a-project-q4".to_string();
    entry.main_integrated = false;
    assert_eq!(compute_status(&entry), CapabilityStatus::Reference);
    let out = validate_entry(entry).expect("must be allowed");
    assert_eq!(out.status, CapabilityStatus::Reference);
}

/// 受入 case 4: 欠落 checksum → activation 不可。M04。
#[test]
fn m04_missing_checksum_not_activatable() {
    let mut entry = base_entry("no-sha");
    entry.sha256 = "".to_string();
    let err = validate_entry(entry).expect_err("missing checksum must be rejected");
    assert!(matches!(err, CatalogError::MissingChecksum(_)));
}

/// fixture catalog.json を読み込み、全エントリが検証される。M04。
#[test]
fn m04_load_catalog_fixture() {
    let entries = load_and_validate(&catalog_path()).expect("catalog must load");
    // 3 エントリ（ds4-main / dspark-tp / a-proj-q4）。M04。
    assert_eq!(entries.len(), 3);
    // a-proj-q4 は未 main → reference。M04。
    let aproj = entries
        .iter()
        .find(|e| e.catalog_id == "a-proj-q4-20260830")
        .expect("a-proj entry");
    assert_eq!(aproj.status, CapabilityStatus::Reference);
    // ds4-main は main 済 → candidate。M04。
    let ds4 = entries
        .iter()
        .find(|e| e.catalog_id == "ds4-main-20260907")
        .expect("ds4 entry");
    assert_eq!(ds4.status, CapabilityStatus::Candidate);
}

/// RAM/size は upstream reference と明示され、性能再現を登録条件にしない。M04。
#[test]
fn m04_ram_size_are_upstream_reference() {
    let entries = load_and_validate(&catalog_path()).expect("catalog must load");
    for e in &entries {
        // RAM reference が明示されている（upstream reference と注記）。M04。
        let ram = e.ram_reference.as_deref().expect("ram_reference present");
        assert!(
            ram.contains("upstream reference") || ram.contains("reference"),
            "ram_reference must be marked as upstream reference: {ram}"
        );
        // size が 0 でない（download 候補として登録）。M04。
        assert!(e.size > 0);
    }
}
