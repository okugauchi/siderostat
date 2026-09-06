//! v0.4.0 操作方針（OperationPolicy）の共通型（C01/C03）。
//!
//! 型の定義のみここで行う。policy の永続動作（journal・epoch・排他）は P01〜P03 が所有する。

/// クラスタの操作方針。メニューバーから自動接続（Automatic）と
/// Standalone 強制（ForcedStandalone）を切り替える。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationPolicy {
    Automatic,
    ForcedStandalone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_kebab_case() {
        assert_eq!(
            serde_json::to_string(&OperationPolicy::Automatic).unwrap(),
            "\"automatic\""
        );
        assert_eq!(
            serde_json::to_string(&OperationPolicy::ForcedStandalone).unwrap(),
            "\"forced-standalone\""
        );
    }

    #[test]
    fn deserializes_kebab_case() {
        assert_eq!(
            serde_json::from_str::<OperationPolicy>("\"automatic\"").unwrap(),
            OperationPolicy::Automatic
        );
        assert_eq!(
            serde_json::from_str::<OperationPolicy>("\"forced-standalone\"").unwrap(),
            OperationPolicy::ForcedStandalone
        );
    }

    #[test]
    fn rejects_unknown_policy() {
        assert!(serde_json::from_str::<OperationPolicy>("\"hybrid\"").is_err());
    }
}
