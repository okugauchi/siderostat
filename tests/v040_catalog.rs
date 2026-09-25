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

/// 配布元・size・full SHA が裏付けられたモデルだけを catalog に含める。M04。
#[test]
fn m04_load_catalog_fixture() {
    let entries = load_and_validate(&catalog_path()).expect("catalog must load");
    // 現在の resource は placeholder URL と仮 checksum だけなので空にする。M04。
    assert!(entries.is_empty());
}

/// placeholder 配布元は入手候補にも verified download にも公開しない。M04。
#[test]
fn m04_placeholder_sources_are_not_downloadable() {
    let entries = load_and_validate(&catalog_path()).expect("catalog must load");
    assert!(entries.is_empty());
}
