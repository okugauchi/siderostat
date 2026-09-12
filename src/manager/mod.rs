//! DS4 Manager — managed registry・job journal・private root。M01。
//!
//! C04（DS4 Manager）に基づき、DS4 source/model の managed namespace を
//! 管理する。互換 root（`~/Library/Application Support/siderostat/`）を維持し、
//! その下に ds4/sources・builds・models・operations・logs を作る。既存外部
//! model と稼働 binary はコピー/削除せず、managed namespace だけを更新する。
//!
//! 契約: CONTRACTS.md C04 / ArtifactRegistry・ManagerJob。M01。
//!
//! 受入 case（全て必須）:
//! - 入力: symlink で root 外 → 拒否
//! - 入力: write 途中 crash → 前 record 読取可（atomic 記録）
//! - 入力: 重複 job → 同 ID
//! - 入力: active digest 不一致 → activation 禁止
//!
//! レビュー重点: フォルダ名だけで trusted と扱わない。削除は本 release で
//! 自動化しない。

pub mod build;
pub mod catalog;
pub mod jobs;
pub mod process;
pub mod registry;
pub mod source;

pub use build::{
    APPROVED_MAKE_TARGETS, APPROVED_ROLES, BuildError, BuildOutcome, BuildRequest, build_artifacts,
    is_approved_role, is_approved_target,
};
pub use catalog::{
    CapabilityStatus, CatalogError, ModelCatalogEntry, compute_status, load_and_validate,
    validate_entry,
};
pub use jobs::{JobKind, ManagerJob, ManagerJobError};
pub use process::{CommandSpec, GroupRunner, ProcessError, RunOutput, RunStatus};
pub use registry::{
    ArtifactRegistry, ArtifactState, BuildRecord, CatalogEntry, ManagedPaths, ManagerRoot,
    RegistryError, SourceRecord,
};
pub use source::{GitRunner, OfficialRemote, SourceError, stage_source};
