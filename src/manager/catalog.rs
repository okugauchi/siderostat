//! DS4 Manager — commit 別 model catalog ・組み合わせ。M04。
//!
//! C01（ModelIdentity / CapabilityStatus）と C04（Catalog = URL/redirect
//! allowlist/size/SHA/license/compatibility）に基づき、commit 別の model
//! catalog を実装する。trusted checksum / source / model family の根拠を
//! 確認できる。性能再現を登録条件にしない（サイズ/RAM は upstream
//! reference と明示する）。M04。
//!
//! 受入 case（全て必須）:
//! - 入力: Vision + 0731 support → 拒否
//! - 入力: TP + 無根拠 DSpark → 拒否
//! - 入力: AProjQ4 未 main → reference
//! - 入力: 欠落 checksum → activation 不可
//!
//! レビュー重点: 性能再現を登録条件にしない。サイズ/RAM は upstream
//! reference と明示する。reference PR を stable にしない。SHA 入手不可の
//! 配布物は download 候補であって検証済みではない。M04。
use std::collections::HashSet;
use std::path::Path;

/// capability 状態。C01: Reference/Candidate/Stable/Unsupported。M04。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// 未 main（reference PR 等）。stable にしない。M04。
    #[default]
    Reference,
    /// main 済だが未検証。M04。
    Candidate,
    /// main 済 + verification 済。M04。
    Stable,
    /// 不適合（理由あり）。M04。
    Unsupported,
}

/// commit 別 model catalog の 1 エントリ。M04。
///
/// C01 ModelIdentity（catalog_id/SHA256/size/family/quantization/任意
/// encoder/support/prefix_file digest）と C04 CatalogEntry（URL/redirect
/// allowlist/size/SHA/license/compatibility）を統合する。M04。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalogEntry {
    /// catalog 内の一意 ID（commit 別）。M04。
    pub catalog_id: String,
    /// 取得元 URL。M04。
    pub url: String,
    /// 許可 redirect 先 allowlist。M04。
    pub redirect_allowlist: Vec<String>,
    /// 期待 size（bytes）。M04。
    pub size: u64,
    /// 期待 SHA-256（full SHA 必須。欠落は activation 不可）。M04。
    pub sha256: String,
    /// license。M04。
    pub license: String,
    /// model family（ds4/dspark/glm/vision/a-project-q4 等）。M04。
    pub family: String,
    /// quantization（Q4 等）。M04。
    pub quantization: String,
    /// 任意 encoder の digest。M04。
    pub encoder: Option<String>,
    /// 任意 support model の digest。M04。
    pub support: Option<String>,
    /// 任意 prefix-file の digest。M04。
    pub prefix_file: Option<String>,
    /// 任意 upstream reference（size/RAM の出典。性能再現条件ではない）。M04。
    pub reference: Option<String>,
    /// main 統合済みか（未 main は Reference）。M04。
    pub main_integrated: bool,
    /// 任意 upstream RAM reference（upstream reference と明示）。M04。
    pub ram_reference: Option<String>,
    /// compatibility / restrictions 記述。M04。
    pub compatibility: Vec<String>,
    /// 計算済み status。M04。
    #[serde(skip)]
    pub status: CapabilityStatus,
}

/// catalog 検証エラー。M04。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// catalog を読めない。M04。
    Load(String),
    /// JSON を解釈できない。M04。
    Parse(String),
    /// 欠落 checksum（activation 不可）。M04。
    MissingChecksum(String),
    /// 不適合な組み合わせ（Vision + 0731 support 等）。M04。
    Unsupported(String),
    /// 無根拠な組み合わせ（TP + 無根拠 DSpark 等）。M04。
    Unfounded(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Load(msg) => write!(f, "catalog load failed: {msg}"),
            CatalogError::Parse(msg) => write!(f, "catalog parse failed: {msg}"),
            CatalogError::MissingChecksum(id) => write!(f, "missing checksum: {id}"),
            CatalogError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            CatalogError::Unfounded(msg) => write!(f, "unfounded: {msg}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// 不適合な組み合わせ（受入 case 1）: Vision + 0731 support。M04。
fn vision_0731_unsupported(entry: &ModelCatalogEntry) -> bool {
    let family = entry.family.to_lowercase();
    family.contains("vision")
        && entry
            .support
            .as_deref()
            .map(|s| s.contains("0731"))
            .unwrap_or(false)
}

/// 無根拠な組み合わせ（受入 case 2）: TP 構成 + 無根拠 DSpark。M04。
///
/// DSpark family で TP 構成（quantization または compatibility に "tp"）
/// かつ upstream reference が無い場合は無根拠として拒否。M04。
fn tp_unfounded_dspark(entry: &ModelCatalogEntry) -> bool {
    let family = entry.family.to_lowercase();
    let is_dspark = family.contains("dspark");
    let is_tp = entry.quantization.to_lowercase().contains("tp")
        || entry
            .compatibility
            .iter()
            .any(|c| c.to_lowercase().contains("tp"));
    let has_reference = entry.reference.as_deref().is_some_and(|r| !r.is_empty());
    is_dspark && is_tp && !has_reference
}

/// 1 エントリの status を計算する。M04。
///
/// - 不適合（Vision + 0731 support）→ Unsupported
/// - 未 main → Reference（reference PR を stable にしない）
/// - main 済 未検証 → Candidate
/// - main 済 検証済 → Stable（verification は H 系で）
///   M04。
pub fn compute_status(entry: &ModelCatalogEntry) -> CapabilityStatus {
    if vision_0731_unsupported(entry) {
        return CapabilityStatus::Unsupported;
    }
    if !entry.main_integrated {
        return CapabilityStatus::Reference;
    }
    // main 済。verification は H 系で実施するため、ここでは Candidate。M04。
    CapabilityStatus::Candidate
}

/// 1 エントリを検証する（受入 case 全部）。M04。
///
/// - 欠落 checksum → MissingChecksum（activation 不可）
/// - Vision + 0731 support → Unsupported
/// - TP + 無根拠 DSpark → Unfounded
/// - それ以外 → 計算済み status を entry に反映して Ok
///   M04。
pub fn validate_entry(mut entry: ModelCatalogEntry) -> Result<ModelCatalogEntry, CatalogError> {
    // 欠落 checksum → activation 不可。M04。
    if entry.sha256.trim().is_empty() {
        return Err(CatalogError::MissingChecksum(entry.catalog_id));
    }
    // Vision + 0731 support → 拒否。M04。
    if vision_0731_unsupported(&entry) {
        return Err(CatalogError::Unsupported(format!(
            "{}: vision + 0731 support is not a supported combination",
            entry.catalog_id
        )));
    }
    // TP + 無根拠 DSpark → 拒否。M04。
    if tp_unfounded_dspark(&entry) {
        return Err(CatalogError::Unfounded(format!(
            "{}: TP dspark without upstream reference is unfounded",
            entry.catalog_id
        )));
    }
    entry.status = compute_status(&entry);
    Ok(entry)
}

/// catalog JSON を読み込み、全エントリを検証する。M04。
pub fn load_and_validate(path: &Path) -> Result<Vec<ModelCatalogEntry>, CatalogError> {
    let text = std::fs::read_to_string(path).map_err(|e| CatalogError::Load(e.to_string()))?;
    let entries: Vec<ModelCatalogEntry> =
        serde_json::from_str(&text).map_err(|e| CatalogError::Parse(e.to_string()))?;
    let mut out = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for entry in entries {
        // catalog_id の一意性を検証。M04。
        if !seen.insert(entry.catalog_id.clone()) {
            return Err(CatalogError::Unsupported(format!(
                "duplicate catalog_id: {}",
                entry.catalog_id
            )));
        }
        out.push(validate_entry(entry)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有効な基底エントリ。M04。
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

    /// 受入 case 1: Vision + 0731 support → 拒否（Unsupported 組み合わせ）。M04。
    #[test]
    fn vision_0731_support_rejected() {
        let mut entry = base_entry("vision-0731");
        entry.family = "vision".to_string();
        entry.support = Some("0731".to_string());
        let err = validate_entry(entry).expect_err("vision+0731 must be rejected");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// 受入 case 2: TP + 無根拠 DSpark → 拒否（Unfounded）。M04。
    #[test]
    fn tp_unfounded_dspark_rejected() {
        let mut entry = base_entry("dspark-tp-no-ref");
        entry.family = "dspark".to_string();
        entry.compatibility = vec!["tp".to_string()];
        entry.reference = None; // 無根拠。M04。
        let err = validate_entry(entry).expect_err("tp dspark without reference must be rejected");
        assert!(matches!(err, CatalogError::Unfounded(_)));
    }

    /// 受入 case 2b: TP + DSpark で reference あり → 許可（Candidate）。M04。
    #[test]
    fn tp_dspark_with_reference_allowed() {
        let mut entry = base_entry("dspark-tp-ref");
        entry.family = "dspark".to_string();
        entry.compatibility = vec!["tp".to_string()];
        entry.reference = Some("https://github.com/example/dspark/pull/42".to_string());
        let out = validate_entry(entry).expect("must be allowed");
        assert_eq!(out.status, CapabilityStatus::Candidate);
    }

    /// 受入 case 3: AProjQ4 未 main → reference（reference PR を stable にしない）。M04。
    #[test]
    fn unmain_integrated_is_reference() {
        let mut entry = base_entry("a-proj-q4");
        entry.family = "a-project-q4".to_string();
        entry.main_integrated = false;
        let out = validate_entry(entry).expect("must be allowed");
        assert_eq!(out.status, CapabilityStatus::Reference);
    }

    /// 受入 case 4: 欠落 checksum → activation 不可（MissingChecksum）。M04。
    #[test]
    fn missing_checksum_not_activatable() {
        let mut entry = base_entry("no-sha");
        entry.sha256 = "   ".to_string();
        let err = validate_entry(entry).expect_err("missing checksum must be rejected");
        assert!(matches!(err, CatalogError::MissingChecksum(_)));
    }

    /// main 済 未検証 → Candidate（Stable にしない）。M04。
    #[test]
    fn main_integrated_unverified_is_candidate() {
        let entry = base_entry("ds4-main");
        let out = validate_entry(entry).expect("must be allowed");
        assert_eq!(out.status, CapabilityStatus::Candidate);
    }
}
