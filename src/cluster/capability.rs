//! v0.4.0 capability manifest の status（C01）。
//!
//! 型の定義のみここで行う。capability 判定ロジック（main 由来・role binary/help・
//! model compatibility・verification）は T01 が所有する。

/// capability の状態。機能ごとに独立して評価し、他の正常 capability を巻き添えにしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    /// upstream main に未統合（PR 等）。実装はあるが本流ではない。
    Reference,
    /// main 由来だが role binary/help/model compatibility の verification 未完了。
    Candidate,
    /// main 由来かつ verification 完了で使用可能。
    Stable,
    /// 現在の hardware/OS/model に不適合。理由を保持する。
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_kebab_case() {
        for (status, expected) in [
            (CapabilityStatus::Reference, "\"reference\""),
            (CapabilityStatus::Candidate, "\"candidate\""),
            (CapabilityStatus::Stable, "\"stable\""),
            (CapabilityStatus::Unsupported, "\"unsupported\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }

    #[test]
    fn deserializes_kebab_case() {
        assert_eq!(
            serde_json::from_str::<CapabilityStatus>("\"candidate\"").unwrap(),
            CapabilityStatus::Candidate
        );
    }

    #[test]
    fn rejects_unknown_status() {
        assert!(serde_json::from_str::<CapabilityStatus>("\"active\"").is_err());
    }
}
