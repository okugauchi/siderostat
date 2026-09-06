//! v0.4.0 共通操作型（C01〜C04 の newtype）。
//!
//! 型の定義のみここで行う。policy の永続動作は P01、capability 判定は T01、
//! TP lifecycle は T06〜T08 が所有する。consumer で再宣言しない。

use uuid::Uuid;

/// 単一の lifecycle 操作を識別する ID。重複抑止・冪等性のために用いる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub Uuid);

/// 操作方針（policy）の世代。stale な control/peer を拒否するために用いる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PolicyEpoch(pub u64);

/// TP（Tensor Parallelism）セッションを識別する ID。role swap / child 交換後に
/// 旧セッションの成功を受理しないために用いる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TpSessionId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_is_opaque_but_comparable() {
        let a = OperationId(Uuid::new_v4());
        let b = OperationId(a.0);
        assert_eq!(a, b);
        assert_ne!(a, OperationId(Uuid::new_v4()));
    }

    #[test]
    fn policy_epoch_orders_so_stale_is_detectable() {
        assert!(PolicyEpoch(1) > PolicyEpoch(0));
        assert!(PolicyEpoch(0) < PolicyEpoch(42));
        assert_eq!(PolicyEpoch(7), PolicyEpoch(7));
    }

    #[test]
    fn tp_session_id_orders_so_old_session_is_rejected() {
        assert!(TpSessionId(2) > TpSessionId(1));
        assert_eq!(TpSessionId(5), TpSessionId(5));
    }
}
