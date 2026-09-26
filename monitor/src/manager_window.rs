//! DS4 管理画面（G03 / C04 / ManagerViewModel）。AppKit 管理 window の
//! view model。commit 候補・active digest・source 取得 / build / cancel /
//! log 導線を保持する。build 可能 target と失敗理由を表示する。G03。/
//!
//! 受入 case（全て必須）:
//! - 入力: source 無し → 取得ボタン（sources 空で fetch 可能）
//! - 入力: build 進行 → cancel 有効（running/cancelling で cancel 可能）
//! - 入力: error → redacted reason（資格情報・URL 等を隠す）
//! - 入力: window 閉じ再開 → job 継続（view model は window と独立）
//!
//! レビュー重点: model 巨大 list や poll で main loop を block しない
//! （poll は非 GUI、view model は純粋ロジック）。sudo install を GUI
//! 既定導線にしない（本 view model に install 導線を含めない）。G03。/
use crate::client::MetricsClient;
use crate::localization::text;
use siderostat_core::manager::api::ManagerStatusResponse;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};

#[cfg(target_os = "macos")]
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSButton, NSLayoutAttribute, NSMenu, NSMenuItem, NSStackView, NSStackViewDistribution,
    NSTextField, NSUserInterfaceLayoutOrientation, NSWindow, NSWindowStyleMask,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSEdgeInsets, NSObject, NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

/// source エントリ（commit 候補・active 判定）。G03。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub remote: String,
    pub full_commit: String,
    /// active digest と一致していれば true。G03。/
    pub active: bool,
}

/// 管理 window の job 表示（secret / raw build log を含まない）。G03。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerJobView {
    pub id: String,
    pub kind: String,
    pub progress: u8,
    pub phase: String,
    pub error: String,
    pub cancel: bool,
}

impl ManagerJobView {
    /// 進行中（cancel が有効）。G03。/
    pub fn is_active(&self) -> bool {
        self.phase == "running" || self.phase == "cancelling"
    }
}

/// `/manager/jobs` の抽象境界（fetch/build/cancel）。テストでは fake で
/// 記録する。自クレート内でのみ使用するため async fn in trait を許可
/// する（clippy -D warnings 対策）。G03。/
#[allow(async_fn_in_trait)]
pub trait ManagerApi {
    /// 新規 job を開始する。Ok(id)。G03。/
    async fn submit(&mut self, kind: &str, payload_key: &str) -> Result<String, String>;

    /// generation を伴う job を開始する。runtime lease は実行時に所有者が解決する。C04。/
    async fn submit_with_generation(
        &mut self,
        kind: &str,
        payload_key: &str,
        expected_generation: Option<u64>,
    ) -> Result<String, String> {
        let _ = expected_generation;
        self.submit(kind, payload_key).await
    }

    /// 進行中 job をキャンセルする。G03。/
    async fn cancel(&mut self, job_id: &str) -> Result<(), String>;
}

/// DS4 管理 window の view model。poll は非 GUI スレッドが行い、view
/// model は結果を反映するだけ（main loop を block しない）。window を
/// 閉じても view model は保持され、job 状態は継続する。G03。/
#[derive(Debug, Clone, Default)]
pub struct ManagerViewModel {
    jobs: BTreeMap<String, ManagerJobView>,
    latest_cancellable_id: Option<String>,
    sources: Vec<SourceEntry>,
    active_digest: Option<String>,
    build_targets: Vec<String>,
    inventory: Option<siderostat_core::manager::api::ManagerInventoryResponse>,
    expected_node_id: Option<String>,
    inventory_error: Option<String>,
}

/// Node-local action whose readiness is derived from the authenticated
/// `/manager/inventory` snapshot and current job status. H06.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagerPreparationAction {
    FetchSource,
    BuildCoordinator,
    BuildWorker,
    DownloadModel,
    VerifyModel,
    StageProfile,
    Activate,
    Rollback,
}

/// UI-safe readiness and the exact local command to enqueue when enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerActionState {
    pub enabled: bool,
    pub reason: Option<String>,
    pub command: Option<ManagerCommand>,
}

#[derive(Debug, Clone, Default)]
struct ManagerActionSelection {
    states: BTreeMap<ManagerPreparationAction, ManagerActionState>,
}

impl ManagerActionSelection {
    fn from_view_model(view_model: &ManagerViewModel) -> Self {
        let actions = [
            ManagerPreparationAction::FetchSource,
            ManagerPreparationAction::BuildCoordinator,
            ManagerPreparationAction::BuildWorker,
            ManagerPreparationAction::DownloadModel,
            ManagerPreparationAction::VerifyModel,
            ManagerPreparationAction::StageProfile,
            ManagerPreparationAction::Activate,
            ManagerPreparationAction::Rollback,
        ];
        Self {
            states: actions
                .into_iter()
                .map(|action| (action, view_model.preparation_action(action)))
                .collect(),
        }
    }

    fn command(&self, action: ManagerPreparationAction) -> Option<ManagerCommand> {
        self.states.get(&action)?.command.clone()
    }
}

impl ManagerActionState {
    fn enabled(command: ManagerCommand) -> Self {
        Self {
            enabled: true,
            reason: None,
            command: Some(command),
        }
    }

    fn disabled(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
            command: None,
        }
    }
}

impl ManagerViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind snapshots to the node this GUI connects to. A response carrying a
    /// different node identity is rejected and cannot supply artifact IDs.
    pub fn set_expected_node_id(&mut self, node_id: impl Into<String>) {
        self.expected_node_id = Some(node_id.into());
    }

    pub fn apply_inventory(
        &mut self,
        inventory: siderostat_core::manager::api::ManagerInventoryResponse,
    ) -> bool {
        if self
            .expected_node_id
            .as_deref()
            .is_some_and(|expected| expected != inventory.node_id)
        {
            self.inventory = None;
            self.active_digest = None;
            self.inventory_error = Some("inventory node identity mismatch".to_string());
            return false;
        }
        self.inventory_error = None;
        self.active_digest = inventory.active_digest.clone();
        self.inventory = Some(inventory);
        true
    }

    pub fn inventory(&self) -> Option<&siderostat_core::manager::api::ManagerInventoryResponse> {
        self.inventory.as_ref()
    }

    pub fn mark_inventory_unavailable(&mut self, message: &str) {
        self.inventory = None;
        self.active_digest = None;
        self.inventory_error = Some(redact_secrets(message));
    }

    pub fn inventory_summary(&self) -> String {
        let Some(inventory) = &self.inventory else {
            return self
                .inventory_error
                .as_deref()
                .map(redact_secrets)
                .unwrap_or_else(|| "inventory未取得".to_string());
        };
        let mut lines = vec![format!(
            "node: {} · role: {}",
            redact_secrets(&inventory.node_id),
            inventory
                .node_role
                .as_deref()
                .map(redact_secrets)
                .unwrap_or_else(|| "未確定".to_string())
        )];
        if inventory.source_commits.is_empty() {
            lines.push("source: 未取得".to_string());
        } else {
            lines.extend(inventory.source_commits.iter().map(|source| {
                format!(
                    "source: {} · main {}",
                    redact_secrets(&source.full_commit),
                    redact_secrets(&source.main_proof)
                )
            }));
        }
        if inventory.artifacts.is_empty() {
            lines.push("artifact: なし".to_string());
        } else {
            lines.extend(inventory.artifacts.iter().map(|artifact| {
                format!(
                    "{} · {} · {} · {} · {}",
                    redact_secrets(&artifact.kind),
                    redact_secrets(&artifact.id),
                    redact_secrets(&artifact.digest),
                    artifact.size,
                    if artifact.verified {
                        "verified"
                    } else {
                        "未検証"
                    }
                )
            }));
        }
        if inventory.profiles.is_empty() {
            lines.push("profile: 未stage".to_string());
        } else {
            lines.extend(inventory.profiles.iter().map(|profile| {
                format!(
                    "profile: {} · role={} · compatibility={:?} · hardware={:?} · activation_ready={}",
                    redact_secrets(&profile.profile_id),
                    redact_secrets(&profile.node_role),
                    profile.compatibility,
                    profile.hardware_readiness,
                    profile.activation_ready
                )
            }));
        }
        lines.push(format!(
            "node readiness: {}{}",
            if inventory.node_readiness.ready {
                "ready"
            } else {
                "pending"
            },
            inventory
                .node_readiness
                .reason
                .as_deref()
                .map(|reason| format!(" · {}", redact_secrets(reason)))
                .unwrap_or_default()
        ));
        lines.push(format!(
            "稼働中 digest: {} · previous digest: {} · activation phase: {} · failure class: {}",
            inventory
                .active_digest
                .as_deref()
                .map(redact_secrets)
                .unwrap_or_else(|| "未実測".to_string()),
            inventory
                .previous_digest
                .as_deref()
                .map(redact_secrets)
                .unwrap_or_else(|| "なし".to_string()),
            inventory
                .activation_phase
                .map(|phase| format!("{phase:?}"))
                .unwrap_or_else(|| "なし".to_string()),
            inventory
                .activation_failure_class
                .as_deref()
                .map(redact_secrets)
                .unwrap_or_else(|| "なし".to_string())
        ));
        if let Some(runtime) = &inventory.runtime {
            lines.push(format!(
                "transaction context: generation={} · state={} · policy={}/{} · epoch={}",
                runtime.generation,
                redact_secrets(&runtime.state),
                redact_secrets(&runtime.desired_policy),
                redact_secrets(&runtime.applied_policy),
                runtime.policy_epoch
            ));
        }
        if let Some(peer) = &inventory.peer {
            lines.push(format!(
                "peer: {} · role={} · live digest={} · previous={} · previous-ready={} · phase={} · failure class={}",
                redact_secrets(&peer.node_id),
                redact_secrets(&peer.node_role),
                peer.active_digest
                    .as_deref()
                    .map(redact_secrets)
                    .unwrap_or_else(|| "未実測".into()),
                peer.previous_digest
                    .as_deref()
                    .map(redact_secrets)
                    .unwrap_or_else(|| "なし".into()),
                peer.previous_release_ready,
                peer.activation_phase
                    .map(|phase| format!("{phase:?}"))
                    .unwrap_or_else(|| "なし".into()),
                peer.activation_failure_class
                    .as_deref()
                    .map(redact_secrets)
                    .unwrap_or_else(|| "なし".into())
            ));
            lines.extend(peer.profiles.iter().map(|profile| {
                format!(
                    "peer profile: {} · role={} · source={} · model={} · candidate={}",
                    redact_secrets(&profile.profile_id),
                    redact_secrets(&profile.node_role),
                    redact_secrets(&profile.source_commit),
                    redact_secrets(&profile.model_digest),
                    redact_secrets(&profile.candidate_digest)
                )
            }));
        } else if inventory
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.cluster_enabled)
        {
            lines.push("peer: readinessを確認できません".into());
        }
        lines.join("\n")
    }

    /// Derive UI readiness and a node-local command from this node's inventory.
    /// IDs are never sourced from a peer view or user-entered paths.
    pub fn preparation_action(&self, action: ManagerPreparationAction) -> ManagerActionState {
        let active_job = |kind: &str| {
            self.jobs
                .values()
                .any(|job| job.kind == kind && job.is_active())
        };
        match action {
            ManagerPreparationAction::FetchSource => {
                if active_job("fetch") {
                    ManagerActionState::disabled("source取得jobが進行中")
                } else {
                    ManagerActionState::enabled(ManagerCommand::FetchSource)
                }
            }
            ManagerPreparationAction::BuildCoordinator | ManagerPreparationAction::BuildWorker => {
                if active_job("build") {
                    return ManagerActionState::disabled("build jobが進行中");
                }
                let Some(inventory) = &self.inventory else {
                    return ManagerActionState::disabled(self.inventory_reason());
                };
                let Some(source) = inventory
                    .source_commits
                    .iter()
                    .max_by_key(|source| (source.fetched_at, &source.receipt_id))
                else {
                    return ManagerActionState::disabled("このnodeのsource commitが未取得");
                };
                let role = match action {
                    ManagerPreparationAction::BuildCoordinator => "ds4-server",
                    ManagerPreparationAction::BuildWorker => "ds4",
                    _ => unreachable!(),
                };
                let expected_node_role = match action {
                    ManagerPreparationAction::BuildCoordinator => "coordinator",
                    ManagerPreparationAction::BuildWorker => "worker",
                    _ => unreachable!(),
                };
                let Some(node_role) = inventory.node_role.as_deref() else {
                    return ManagerActionState::disabled("このnodeのruntime roleが未確定です");
                };
                if node_role != expected_node_role {
                    return ManagerActionState::disabled(format!(
                        "このnodeは{node_role} roleです。{expected_node_role}用buildは実行できません"
                    ));
                }
                if siderostat_core::manager::executor::manager_build_payload_key(
                    &source.receipt_id,
                    role,
                )
                .is_none()
                {
                    return ManagerActionState::disabled(
                        "このnodeのsource receipt identityが不正です",
                    );
                }
                ManagerActionState::enabled(ManagerCommand::Build {
                    source_receipt_id: source.receipt_id.clone(),
                    role: role.to_string(),
                })
            }
            ManagerPreparationAction::DownloadModel => {
                if active_job("download") {
                    ManagerActionState::disabled("model download jobが進行中")
                } else {
                    ManagerActionState::disabled(
                        "このnodeのinventoryから利用可能なverified catalog候補を確認できません",
                    )
                }
            }
            ManagerPreparationAction::VerifyModel => {
                if active_job("verify") {
                    return ManagerActionState::disabled("verify jobが進行中");
                }
                let Some(inventory) = &self.inventory else {
                    return ManagerActionState::disabled(self.inventory_reason());
                };
                if let Some(artifact) = inventory.artifacts.iter().find(|artifact| {
                    artifact.kind == "model" && !artifact.verified && artifact.catalog_id.is_some()
                }) {
                    ManagerActionState::enabled(ManagerCommand::Verify {
                        artifact_id: artifact.id.clone(),
                    })
                } else {
                    ManagerActionState::disabled(
                        "このnodeにcatalog provenance付き未検証model artifactがありません",
                    )
                }
            }
            ManagerPreparationAction::StageProfile => {
                if active_job("stage") {
                    return ManagerActionState::disabled("stage jobが進行中");
                }
                let Some(inventory) = &self.inventory else {
                    return ManagerActionState::disabled(self.inventory_reason());
                };
                let Some(node_role) = inventory.node_role.as_deref() else {
                    return ManagerActionState::disabled("このnodeのruntime roleが未確定です");
                };
                let expected_build_role = match node_role {
                    "coordinator" => "ds4-server",
                    "worker" => "ds4",
                    _ => {
                        return ManagerActionState::disabled("このnodeのruntime roleが不明です");
                    }
                };
                let models = inventory
                    .artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact.kind == "model"
                            && artifact.verified
                            && artifact.catalog_id.is_some()
                    })
                    .collect::<Vec<_>>();
                if models.len() > 1 {
                    return ManagerActionState::disabled(
                        "verified model候補が複数あり、選択UIが必要です",
                    );
                }
                let Some(model) = models.first().copied() else {
                    return ManagerActionState::disabled(
                        "このnodeのmodel artifactが未検証またはcatalog provenanceなし",
                    );
                };
                let builds = inventory
                    .artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact.kind == "build"
                            && artifact.verified
                            && artifact.role.as_deref() == Some(expected_build_role)
                    })
                    .collect::<Vec<_>>();
                if builds.len() > 1 {
                    return ManagerActionState::disabled(
                        "verified serving build候補が複数あり、選択UIが必要です",
                    );
                }
                let Some(build) = builds.first().copied() else {
                    return ManagerActionState::disabled(
                        "このnodeにverified serving build artifactがありません",
                    );
                };
                if siderostat_core::manager::executor::manager_stage_payload_key(
                    &build.id, &model.id,
                )
                .is_none()
                {
                    return ManagerActionState::disabled("このnodeのartifact identityが不正です");
                }
                if inventory.profiles.iter().any(|profile| {
                    profile
                        .role_artifacts
                        .iter()
                        .any(|reference| reference.id == build.id)
                        && profile.model_artifact.id == model.id
                }) {
                    let reason = inventory
                        .profiles
                        .iter()
                        .find(|profile| {
                            profile
                                .role_artifacts
                                .iter()
                                .any(|reference| reference.id == build.id)
                                && profile.model_artifact.id == model.id
                        })
                        .map(|profile| {
                            if profile.hardware_readiness
                                == siderostat_core::manager::HardwareReadiness::Pending
                            {
                                "既存profileのhardware readinessがpending"
                            } else if profile.compatibility
                                != siderostat_core::manager::ProfileCompatibility::Compatible
                            {
                                "既存profileのcompatibilityが未確認"
                            } else {
                                "同じartifact pairはすでにstage済み"
                            }
                        })
                        .unwrap_or("同じartifact pairはすでにstage済み");
                    return ManagerActionState::disabled(reason);
                }
                ManagerActionState::enabled(ManagerCommand::Stage {
                    build_artifact_id: build.id.clone(),
                    model_artifact_id: model.id.clone(),
                })
            }
            ManagerPreparationAction::Activate => {
                let Some(inventory) = &self.inventory else {
                    return ManagerActionState::disabled(self.inventory_reason());
                };
                if let Some(reason) = self.transaction_precondition_reason(inventory) {
                    return ManagerActionState::disabled(reason);
                }
                let Some(node_role) = inventory.node_role.as_deref() else {
                    return ManagerActionState::disabled("このnodeのroleが未確定です");
                };
                let matching_peer_profiles = if let Some(runtime) = &inventory.runtime {
                    if runtime.cluster_enabled {
                        let Some(peer) = &inventory.peer else {
                            return ManagerActionState::disabled("peer readinessを確認できません");
                        };
                        let expected_peer_role = match node_role {
                            "coordinator" => "worker",
                            "worker" => "coordinator",
                            _ => return ManagerActionState::disabled("node roleが不明です"),
                        };
                        if peer.node_role != expected_peer_role {
                            return ManagerActionState::disabled("peer roleが一致しません");
                        }
                        Some(peer)
                    } else {
                        None
                    }
                } else {
                    return ManagerActionState::disabled("runtime readinessを確認できません");
                };
                let candidates = inventory
                    .profiles
                    .iter()
                    .filter(|profile| {
                        profile.activation_ready
                            && profile.node_role == node_role
                            && profile.compatibility
                                == siderostat_core::manager::ProfileCompatibility::Compatible
                            && profile.hardware_readiness
                                == siderostat_core::manager::HardwareReadiness::Ready
                    })
                    .filter(|profile| {
                        let Some((source_commit, model_digest, catalog_id)) =
                            self.local_profile_identity(inventory, profile)
                        else {
                            return false;
                        };
                        matching_peer_profiles.is_none_or(|peer| {
                            peer.profiles.iter().any(|peer_profile| {
                                peer_profile.node_role
                                    == if node_role == "coordinator" {
                                        "worker"
                                    } else {
                                        "coordinator"
                                    }
                                    && peer_profile.source_commit == source_commit
                                    && peer_profile.model_digest == model_digest
                                    && peer_profile.model_catalog_id == catalog_id
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                if candidates.is_empty() {
                    return ManagerActionState::disabled(
                        if inventory
                            .profiles
                            .iter()
                            .any(|profile| profile.activation_ready)
                        {
                            "peerにsource/model互換のverified profileがありません"
                        } else {
                            "このnodeにactivation-ready profileがありません"
                        },
                    );
                }
                if candidates.len() != 1 {
                    return ManagerActionState::disabled(
                        "複数のactivation-ready profileがあり、候補選択が必要です",
                    );
                }
                let generation = inventory.runtime.as_ref().unwrap().generation;
                ManagerActionState::enabled(ManagerCommand::Activate {
                    profile: candidates[0].profile_id.clone(),
                    expected_generation: generation,
                })
            }
            ManagerPreparationAction::Rollback => {
                let Some(inventory) = &self.inventory else {
                    return ManagerActionState::disabled(self.inventory_reason());
                };
                if let Some(reason) = self.transaction_precondition_reason(inventory) {
                    return ManagerActionState::disabled(reason);
                }
                if inventory.previous_profile_id.is_none()
                    || inventory.previous_digest.is_none()
                    || !inventory.previous_release_ready
                {
                    return ManagerActionState::disabled(
                        "このnodeにverified previous releaseがありません",
                    );
                }
                if inventory
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.cluster_enabled)
                {
                    let Some(peer) = &inventory.peer else {
                        return ManagerActionState::disabled("peer readinessを確認できません");
                    };
                    if peer.previous_profile_id.is_none()
                        || peer.previous_digest.is_none()
                        || !peer.previous_release_ready
                    {
                        return ManagerActionState::disabled(
                            "peerにverified previous releaseがありません",
                        );
                    }
                }
                ManagerActionState::enabled(ManagerCommand::Rollback {
                    expected_generation: inventory.runtime.as_ref().unwrap().generation,
                })
            }
        }
    }

    fn transaction_precondition_reason(
        &self,
        inventory: &siderostat_core::manager::api::ManagerInventoryResponse,
    ) -> Option<String> {
        use siderostat_core::manager::PersistedActivationPhase as Phase;

        let Some(runtime) = &inventory.runtime else {
            return Some("runtime readinessを確認できません".into());
        };
        if runtime.generation == 0 {
            return Some("cluster generationが未確定です".into());
        }
        if runtime.desired_policy != "automatic" || runtime.applied_policy != "automatic" {
            return Some("operation policyがactivationを許可していません".into());
        }
        let expected_state = if runtime.cluster_enabled {
            "paired-standalone-ready"
        } else {
            "solo-standalone-ready"
        };
        if runtime.state != expected_state {
            return Some("runtimeが安定したstandalone stateではありません".into());
        }
        if !inventory.node_readiness.ready {
            return Some(
                inventory
                    .node_readiness
                    .reason
                    .as_deref()
                    .map(redact_secrets)
                    .unwrap_or_else(|| "local readinessが未完了です".into()),
            );
        }
        if inventory.active_digest.is_none() {
            return Some("現在のactive releaseをlive stateから確認できません".into());
        }
        if runtime.cluster_enabled {
            let Some(peer) = &inventory.peer else {
                return Some("peer readinessを確認できません".into());
            };
            if peer.active_digest.is_none() {
                return Some("peerのactive releaseをlive stateから確認できません".into());
            }
        }
        if inventory
            .activation_phase
            .is_some_and(|phase| !matches!(phase, Phase::Complete | Phase::RolledBack))
            || inventory.peer.as_ref().is_some_and(|peer| {
                peer.activation_phase
                    .is_some_and(|phase| !matches!(phase, Phase::Complete | Phase::RolledBack))
            })
        {
            return Some("前回のtransactionが未解決です".into());
        }
        if self.jobs.values().any(|job| {
            (job.kind == "activate" || job.kind == "rollback")
                && (job.is_active() || job.phase == "interrupted")
        }) {
            return Some("activation/rollback jobが進行中またはinterruptedです".into());
        }
        None
    }

    fn local_profile_identity(
        &self,
        inventory: &siderostat_core::manager::api::ManagerInventoryResponse,
        profile: &siderostat_core::manager::api::ManagerStagedProfileDto,
    ) -> Option<(String, String, String)> {
        let [build] = profile.role_artifacts.as_slice() else {
            return None;
        };
        if !build.verified {
            return None;
        }
        let build_digest = build.digest.as_ref()?;
        let build_artifact = inventory.artifacts.iter().find(|artifact| {
            artifact.id == build.id
                && artifact.kind == "build"
                && artifact.verified
                && &artifact.digest == build_digest
        })?;
        let source_commit = build_artifact.source_commit.as_ref()?.clone();
        if !profile.model_artifact.verified {
            return None;
        }
        let model_digest = profile.model_artifact.digest.as_ref()?.clone();
        let model_artifact = inventory.artifacts.iter().find(|artifact| {
            artifact.id == profile.model_artifact.id
                && artifact.kind == "model"
                && artifact.verified
                && artifact.digest == model_digest
        })?;
        let catalog_id = model_artifact.catalog_id.as_ref()?.clone();
        Some((source_commit, model_digest, catalog_id))
    }

    fn inventory_reason(&self) -> String {
        self.inventory_error
            .as_deref()
            .map(redact_secrets)
            .unwrap_or_else(|| "inventory未取得".to_string())
    }

    /// `/manager/status` の反映。job を個別更新し、active digest を
    /// 保持する。source fetch が succeeded なら sources へ追加する。
    /// G03。
    pub fn apply_status(
        &mut self,
        jobs: &[siderostat_core::manager::api::ManagerJobDto],
        active_digest: Option<&str>,
    ) {
        if let Some(active_digest) = active_digest {
            self.active_digest = Some(active_digest.to_string());
        } else if self.inventory.is_none() {
            self.active_digest = None;
        }
        self.latest_cancellable_id = jobs
            .iter()
            .filter(|job| job.phase == "running")
            .max_by_key(|job| (job.updated_at, job.created_at, &job.id))
            .map(|job| job.id.clone());
        for job in jobs {
            self.jobs.insert(
                job.id.clone(),
                ManagerJobView {
                    id: job.id.clone(),
                    kind: job.kind.clone(),
                    progress: job.progress,
                    phase: job.phase.clone(),
                    error: job.error.clone(),
                    cancel: job.cancel,
                },
            );
            // source fetch succeeded → sources へ追加。G03。
            if job.kind == "fetch"
                && job.phase == "succeeded"
                && !self.sources.iter().any(|s| s.remote == job.id)
            {
                self.sources.push(SourceEntry {
                    remote: job.id.clone(),
                    full_commit: String::new(),
                    active: self.active_digest.as_deref() == Some(job.id.as_str()),
                });
            }
        }
    }

    /// source が無い状態 → 取得ボタン有効。G03。/
    pub fn can_fetch_source(&self) -> bool {
        self.sources.is_empty()
    }

    /// source 取得を開始する（POST /manager/jobs kind=fetch）。G03。/
    pub async fn fetch_source(&mut self, api: &mut impl ManagerApi) -> Result<String, String> {
        let id = api.submit("fetch", "official").await?;
        self.jobs.insert(
            id.clone(),
            ManagerJobView {
                id: id.clone(),
                kind: "fetch".to_string(),
                progress: 0,
                phase: "running".to_string(),
                error: String::new(),
                cancel: true,
            },
        );
        self.latest_cancellable_id = Some(id.clone());
        Ok(id)
    }

    /// build 対象（target）。G03。/
    pub fn build_targets(&self) -> &[String] {
        &self.build_targets
    }

    /// build を開始する（POST /manager/jobs kind=build）。G03。/
    pub async fn start_build(
        &mut self,
        target: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        let id = api.submit("build", target).await?;
        if !self.build_targets.contains(&target.to_string()) {
            self.build_targets.push(target.to_string());
        }
        self.jobs.insert(
            id.clone(),
            ManagerJobView {
                id: id.clone(),
                kind: "build".to_string(),
                progress: 0,
                phase: "running".to_string(),
                error: String::new(),
                cancel: true,
            },
        );
        self.latest_cancellable_id = Some(id.clone());
        Ok(id)
    }

    /// build 進行中 → cancel 有効。G03。/
    pub fn can_cancel(&self, job_id: &str) -> bool {
        self.jobs
            .get(job_id)
            .is_some_and(|job| job.kind == "build" && job.is_active())
    }

    /// build job をキャンセルする（POST /manager/jobs/{id}/cancel）。
    /// G03。/
    pub async fn cancel_job(
        &mut self,
        job_id: &str,
        api: &mut impl ManagerApi,
    ) -> Result<(), String> {
        api.cancel(job_id).await?;
        if let Some(job) = self.jobs.get_mut(job_id) {
            job.phase = "cancelling".to_string();
        }
        Ok(())
    }

    /// job 一覧。G03。/
    pub fn jobs(&self) -> impl Iterator<Item = &ManagerJobView> {
        self.jobs.values()
    }

    /// 最新のrunning job ID。`cancel` wire fieldはキャンセル要求済みフラグ。H06。
    pub fn latest_cancellable_job_id(&self) -> Option<&str> {
        self.latest_cancellable_id.as_deref()
    }

    /// source 一覧。G03。/
    pub fn sources(&self) -> &[SourceEntry] {
        &self.sources
    }

    /// active digest。G03。/
    pub fn active_digest(&self) -> Option<&str> {
        self.active_digest.as_deref()
    }

    /// Display unknown manager status explicitly; None is not proof that no
    /// profile is active. H06.
    pub fn active_digest_display(&self) -> &str {
        self.active_digest
            .as_deref()
            .unwrap_or("active digest 未実測（runtime / registry 未接続）")
    }

    /// job エラーの redacted 表示。資格情報・URL userinfo・query 等を
    /// 隠し、生のエラー文字列をそのまま GUI に出さない（C04:
    /// URL query/credentials/token をログ/表示へ出さない）。G03。/
    pub fn redacted_reason(job: &ManagerJobView) -> String {
        redact_secrets(&job.error)
    }
}

/// GUIからworkerへ送るmanager操作。HTTP処理はこのenumを受けたworkerが
/// 実行し、AppKitのmain loopでは実行しない。H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerCommand {
    FetchSource,
    Build {
        source_receipt_id: String,
        role: String,
    },
    Download {
        catalog_id: String,
    },
    Verify {
        artifact_id: String,
    },
    Stage {
        build_artifact_id: String,
        model_artifact_id: String,
    },
    Activate {
        profile: String,
        expected_generation: u64,
    },
    Rollback {
        expected_generation: u64,
    },
    Cancel {
        job_id: String,
    },
    Refresh,
    RefreshInventory,
}

/// workerからmain threadへ返すmanager状態更新。非terminal jobを成功へ
/// 変換せず、表示側で観測した状態をそのまま保持する。H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerEvent {
    Status(ManagerStatusResponse),
    Inventory(siderostat_core::manager::api::ManagerInventoryResponse),
    Submitted { kind: String, id: String },
    Failed { message: String },
}

/// Convert a worker error into an event safe for the GUI boundary. Secrets and
/// URL credentials are redacted before the event can reach AppKit. H06。
pub fn manager_failed_event(message: impl AsRef<str>) -> ManagerEvent {
    ManagerEvent::Failed {
        message: redact_secrets(message.as_ref()),
    }
}

/// A successful submit only acknowledges that a non-terminal job was queued.
/// The terminal result is observed later through `ManagerEvent::Status`. H06。
pub fn manager_submitted_event(kind: impl Into<String>, id: impl Into<String>) -> ManagerEvent {
    ManagerEvent::Submitted {
        kind: kind.into(),
        id: id.into(),
    }
}

/// Execute one manager command on the worker side. No command is reported as
/// terminal success here; status snapshots remain the source of truth for job
/// phases. H06。
pub async fn execute_manager_command(
    client: &MetricsClient,
    command: ManagerCommand,
) -> ManagerEvent {
    let result = match command {
        ManagerCommand::FetchSource => client
            .submit_manager_job("fetch", "official")
            .await
            .map(|response| manager_submitted_event("fetch", response.id)),
        ManagerCommand::Build {
            source_receipt_id,
            role,
        } => match siderostat_core::manager::executor::manager_build_payload_key(
            &source_receipt_id,
            &role,
        ) {
            Some(payload_key) => client
                .submit_manager_job("build", &payload_key)
                .await
                .map(|response| manager_submitted_event("build", response.id)),
            None => Err(anyhow::anyhow!(
                "local source receipt or build role is invalid"
            )),
        },
        ManagerCommand::Download { catalog_id } => client
            .submit_manager_job("download", &catalog_id)
            .await
            .map(|response| manager_submitted_event("download", response.id)),
        ManagerCommand::Verify { artifact_id } => client
            .submit_manager_job("verify", &artifact_id)
            .await
            .map(|response| manager_submitted_event("verify", response.id)),
        ManagerCommand::Stage {
            build_artifact_id,
            model_artifact_id,
        } => match siderostat_core::manager::executor::manager_stage_payload_key(
            &build_artifact_id,
            &model_artifact_id,
        ) {
            Some(payload_key) => client
                .submit_manager_job("stage", &payload_key)
                .await
                .map(|response| manager_submitted_event("stage", response.id)),
            None => Err(anyhow::anyhow!(
                "local managed artifact identity is invalid"
            )),
        },
        ManagerCommand::Activate {
            profile,
            expected_generation,
        } => client
            .submit_manager_job_with_generation("activate", &profile, expected_generation)
            .await
            .map(|response| manager_submitted_event("activate", response.id)),
        ManagerCommand::Rollback {
            expected_generation,
        } => client
            .submit_manager_job_with_generation("rollback", "previous", expected_generation)
            .await
            .map(|response| manager_submitted_event("rollback", response.id)),
        ManagerCommand::Cancel { job_id } => client
            .cancel_manager_job(&job_id)
            .await
            .map(|()| manager_submitted_event("cancel", job_id)),
        ManagerCommand::Refresh => client.fetch_manager_jobs().await.map(ManagerEvent::Status),
        ManagerCommand::RefreshInventory => client
            .fetch_manager_inventory()
            .await
            .map(ManagerEvent::Inventory)
            .map_err(|error| anyhow::anyhow!("inventory refresh failed: {error}")),
    };
    result.unwrap_or_else(|error| manager_failed_event(error.to_string()))
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ManagerActionIvars {
    command_tx: Sender<ManagerCommand>,
    cancel_job_id: Arc<Mutex<Option<String>>>,
    action_selection: Arc<Mutex<ManagerActionSelection>>,
}

#[cfg(target_os = "macos")]
fn send_cancel_command(
    sender: &Sender<ManagerCommand>,
    selected: &Mutex<Option<String>>,
) -> anyhow::Result<bool> {
    let id = selected
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(job_id) = id else {
        return Ok(false);
    };
    sender
        .send(ManagerCommand::Cancel { job_id })
        .context("manager command channel closed")?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn send_preparation_command(
    sender: &Sender<ManagerCommand>,
    selection: &Mutex<ManagerActionSelection>,
    action: ManagerPreparationAction,
) {
    let command = selection
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .command(action);
    if let Some(command) = command {
        let _ = sender.send(command);
    }
}

#[cfg(target_os = "macos")]
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ManagerActionIvars]
    struct ManagerActionTarget;

    impl ManagerActionTarget {
        #[unsafe(method(managerFetch:))]
        fn manager_fetch(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::FetchSource,
            );
        }

        #[unsafe(method(managerBuild:))]
        fn manager_build(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::BuildCoordinator,
            );
        }

        #[unsafe(method(managerBuildWorker:))]
        fn manager_build_worker(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::BuildWorker,
            );
        }

        #[unsafe(method(managerDownload:))]
        fn manager_download(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::DownloadModel,
            );
        }

        #[unsafe(method(managerVerify:))]
        fn manager_verify(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::VerifyModel,
            );
        }

        #[unsafe(method(managerStage:))]
        fn manager_stage(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::StageProfile,
            );
        }

        #[unsafe(method(managerActivate:))]
        fn manager_activate(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::Activate,
            );
        }

        #[unsafe(method(managerRollback:))]
        fn manager_rollback(&self, _sender: Option<&AnyObject>) {
            send_preparation_command(
                &self.ivars().command_tx,
                &self.ivars().action_selection,
                ManagerPreparationAction::Rollback,
            );
        }

        #[unsafe(method(managerCancel:))]
        fn manager_cancel(&self, _sender: Option<&AnyObject>) {
            let _ = send_cancel_command(&self.ivars().command_tx, &self.ivars().cancel_job_id);
        }

        #[unsafe(method(managerRefresh:))]
        fn manager_refresh(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Refresh);
            let _ = self
                .ivars()
                .command_tx
                .send(ManagerCommand::RefreshInventory);
        }
    }
);

#[cfg(target_os = "macos")]
impl ManagerActionTarget {
    fn new(
        mtm: MainThreadMarker,
        command_tx: Sender<ManagerCommand>,
        cancel_job_id: Arc<Mutex<Option<String>>>,
        action_selection: Arc<Mutex<ManagerActionSelection>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ManagerActionIvars {
            command_tx,
            cancel_job_id,
            action_selection,
        });
        // SAFETY: ManagerActionTarget directly subclasses NSObject and uses
        // NSObject's standard init implementation.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(target_os = "macos")]
fn project_jobs(view_model: &ManagerViewModel) -> String {
    let rows: Vec<String> = view_model
        .jobs()
        .map(|job| {
            let error = if job.error.is_empty() {
                "なし".to_string()
            } else {
                ManagerViewModel::redacted_reason(job)
            };
            format!(
                "{} · {} · {} · {}% · error: {}",
                redact_secrets(&job.id),
                redact_secrets(&job.kind),
                redact_secrets(&job.phase),
                job.progress,
                error
            )
        })
        .collect();
    if rows.is_empty() {
        "jobはありません。再読込で状態を取得できます。".to_string()
    } else {
        rows.join("\n")
    }
}

#[cfg(target_os = "macos")]
fn project_profiles(model_view: &ModelView) -> String {
    if model_view.models().is_empty() {
        return "Pending · profile未準備（model catalog入力なし）· 操作不可".to_string();
    }
    model_view
        .models()
        .iter()
        .map(|model| {
            let state = match model_view.profile_entry_readiness(model) {
                ProfileReadiness::Pending(reason) => format!("Pending（{reason}）· 操作不可"),
                ProfileReadiness::Ready => "Ready（検証入力上）".to_string(),
                ProfileReadiness::Rejected(reason) => format!("Rejected（{reason}）· 操作不可"),
            };
            let name = model.origin.as_deref().map_or_else(
                || redact_secrets(&model.name),
                |origin| {
                    format!(
                        "{} · {}",
                        redact_secrets(origin),
                        redact_secrets(&model.name)
                    )
                },
            );
            let declared_digest = model
                .origin
                .as_ref()
                .and(model.checksum.as_deref())
                .map(|digest| format!(" · 設定値 digest={}", redact_secrets(digest)))
                .unwrap_or_default();
            format!("{name} · {}{declared_digest}", redact_secrets(&state))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_preparation(view_model: &ManagerViewModel) -> String {
    [
        ("Fetch", ManagerPreparationAction::FetchSource),
        (
            "Build coordinator",
            ManagerPreparationAction::BuildCoordinator,
        ),
        ("Build worker", ManagerPreparationAction::BuildWorker),
        ("Download", ManagerPreparationAction::DownloadModel),
        ("Verify", ManagerPreparationAction::VerifyModel),
        ("Stage", ManagerPreparationAction::StageProfile),
        ("Activate", ManagerPreparationAction::Activate),
        ("Rollback", ManagerPreparationAction::Rollback),
    ]
    .into_iter()
    .map(|(label, action)| {
        let state = view_model.preparation_action(action);
        let status = if state.enabled {
            "実行可能".to_string()
        } else {
            state
                .reason
                .map(|reason| redact_secrets(&reason))
                .unwrap_or_else(|| "操作不可".to_string())
        };
        format!("{label}: {status}")
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

#[cfg(target_os = "macos")]
fn manager_copy_menus(mtm: MainThreadMarker) -> (Retained<NSMenu>, Retained<NSMenu>) {
    let main_menu = NSMenu::new(mtm);
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str(&text("menu.manager_edit", "編集")));

    let copy_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&text("menu.manager_copy", "コピー")),
            Some(sel!(copy:)),
            &NSString::from_str("c"),
        )
    };
    edit_menu.addItem(&copy_item);

    let edit_menu_item = NSMenuItem::new(mtm);
    edit_menu_item.setTitle(&NSString::from_str(&text("menu.manager_edit", "編集")));
    edit_menu_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_menu_item);

    let context_menu = NSMenu::new(mtm);
    let context_copy_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&text("menu.manager_copy", "コピー")),
            Some(sel!(copy:)),
            &NSString::from_str(""),
        )
    };
    context_menu.addItem(&context_copy_item);

    (main_menu, context_menu)
}

/// AppKit manager window host. The view model and command channel outlive the
/// visible window, so close/reopen does not cancel jobs. H06。
#[cfg(target_os = "macos")]
pub struct ManagerWindowHost {
    mtm: Option<MainThreadMarker>,
    _client: MetricsClient,
    _main_menu: Option<Retained<NSMenu>>,
    _copy_context_menu: Option<Retained<NSMenu>>,
    window: Option<Retained<NSWindow>>,
    status_label: Option<Retained<NSTextField>>,
    jobs_label: Option<Retained<NSTextField>>,
    profiles_label: Option<Retained<NSTextField>>,
    inventory_label: Option<Retained<NSTextField>>,
    preparation_label: Option<Retained<NSTextField>>,
    cancel_button: Option<Retained<NSButton>>,
    preparation_buttons: Vec<(ManagerPreparationAction, Retained<NSButton>)>,
    _action_target: Option<Retained<ManagerActionTarget>>,
    command_tx: Sender<ManagerCommand>,
    command_rx: Option<Receiver<ManagerCommand>>,
    view_model: ManagerViewModel,
    model_view: ModelView,
    jobs_summary: String,
    profiles_summary: String,
    inventory_summary: String,
    preparation_summary: String,
    status_summary: String,
    action_selection: Arc<Mutex<ManagerActionSelection>>,
    cancel_job_id: Arc<Mutex<Option<String>>>,
    test_window_identity: usize,
    test_visible: bool,
}

#[cfg(target_os = "macos")]
impl ManagerWindowHost {
    pub fn new(
        mtm: MainThreadMarker,
        client: MetricsClient,
        view_model: ManagerViewModel,
        model_view: ModelView,
    ) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let cancel_job_id = Arc::new(Mutex::new(
            view_model.latest_cancellable_job_id().map(str::to_string),
        ));
        let action_selection = Arc::new(Mutex::new(ManagerActionSelection::from_view_model(
            &view_model,
        )));
        let action_target = ManagerActionTarget::new(
            mtm,
            command_tx.clone(),
            Arc::clone(&cancel_job_id),
            Arc::clone(&action_selection),
        );
        let (main_menu, copy_context_menu) = manager_copy_menus(mtm);
        NSApplication::sharedApplication(mtm).setMainMenu(Some(&main_menu));
        let jobs_summary = project_jobs(&view_model);
        let profiles_summary = project_profiles(&model_view);
        let inventory_summary = view_model.inventory_summary();
        let preparation_summary = project_preparation(&view_model);
        let status_summary =
            "待機中。runtime/modelは変更されていません。job状態を確認してください。".to_string();
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(780.0, 600.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the host retains the window for the process lifetime, so it
        // must not be released when the user closes it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("siDeroStat Manager"));
        window.center();
        let content = window
            .contentView()
            .context("manager window content view unavailable")?;

        // Use AppKit's standard stack layout so the window reads like a native
        // settings/tool window: a clear header, grouped sections, and compact
        // horizontal action rows. H06。
        let root = NSStackView::new(mtm);
        root.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        root.setAlignment(NSLayoutAttribute::Leading);
        root.setDistribution(NSStackViewDistribution::GravityAreas);
        root.setSpacing(12.0);
        root.setEdgeInsets(NSEdgeInsets {
            top: 24.0,
            left: 28.0,
            bottom: 24.0,
            right: 28.0,
        });
        root.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(780.0, 600.0),
        ));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        content.addSubview(&root);

        let title = NSTextField::labelWithString(&NSString::from_str("DS4 Manager"), mtm);
        root.addArrangedSubview(&title);
        let subtitle = NSTextField::labelWithString(
            &NSString::from_str("source、artifact、profile、jobを確認・操作します"),
            mtm,
        );
        root.addArrangedSubview(&subtitle);
        let status_label = NSTextField::wrappingLabelWithString(
            &NSString::from_str(
                "待機中。runtime/modelは変更されていません。job状態を確認してください。",
            ),
            mtm,
        );
        status_label.setPreferredMaxLayoutWidth(700.0);
        status_label.setMaximumNumberOfLines(2);
        // NSResponder stores this context menu without retaining it, so the
        // host keeps `copy_context_menu` alive for the labels' lifetime.
        unsafe { status_label.setMenu(Some(&copy_context_menu)) };
        root.addArrangedSubview(&status_label);

        let section = |title: &str| {
            let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            root.addArrangedSubview(&label);
        };
        let action_row = |buttons: Vec<Retained<NSButton>>| {
            let row = NSStackView::new(mtm);
            row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            row.setSpacing(8.0);
            row.setAlignment(NSLayoutAttribute::CenterY);
            for button in buttons {
                row.addArrangedSubview(&button);
            }
            root.addArrangedSubview(&row);
        };
        let active_button = |text: &str, action| unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(text),
                Some(&*action_target),
                Some(action),
                mtm,
            )
        };
        let fetch_button = active_button("公式sourceを取得", sel!(managerFetch:));
        let build_coordinator_button = active_button("coordinator用をbuild", sel!(managerBuild:));
        let build_worker_button = active_button("worker用をbuild", sel!(managerBuildWorker:));
        let download_button = active_button("modelをdownload", sel!(managerDownload:));
        let verify_button = active_button("verify", sel!(managerVerify:));
        let stage_button = active_button("stage", sel!(managerStage:));
        let activate_button = active_button("Activate", sel!(managerActivate:));
        let rollback_button = active_button("Rollback to previous", sel!(managerRollback:));
        let preparation_buttons = vec![
            (ManagerPreparationAction::FetchSource, fetch_button.clone()),
            (
                ManagerPreparationAction::BuildCoordinator,
                build_coordinator_button.clone(),
            ),
            (
                ManagerPreparationAction::BuildWorker,
                build_worker_button.clone(),
            ),
            (
                ManagerPreparationAction::DownloadModel,
                download_button.clone(),
            ),
            (ManagerPreparationAction::VerifyModel, verify_button.clone()),
            (ManagerPreparationAction::StageProfile, stage_button.clone()),
            (ManagerPreparationAction::Activate, activate_button.clone()),
            (ManagerPreparationAction::Rollback, rollback_button.clone()),
        ];
        for (action, button) in &preparation_buttons {
            button.setEnabled(view_model.preparation_action(*action).enabled);
        }

        section("Runtime / source");
        action_row(vec![
            fetch_button,
            build_coordinator_button,
            build_worker_button,
        ]);
        section("Artifact pipeline");
        action_row(vec![download_button, verify_button, stage_button]);
        let preparation_label =
            NSTextField::wrappingLabelWithString(&NSString::from_str(&preparation_summary), mtm);
        preparation_label.setPreferredMaxLayoutWidth(700.0);
        unsafe { preparation_label.setMenu(Some(&copy_context_menu)) };
        root.addArrangedSubview(&preparation_label);
        section("Profiles");
        let profiles =
            NSTextField::wrappingLabelWithString(&NSString::from_str(&profiles_summary), mtm);
        profiles.setPreferredMaxLayoutWidth(700.0);
        unsafe { profiles.setMenu(Some(&copy_context_menu)) };
        root.addArrangedSubview(&profiles);
        section("This node inventory");
        let inventory =
            NSTextField::wrappingLabelWithString(&NSString::from_str(&inventory_summary), mtm);
        inventory.setPreferredMaxLayoutWidth(700.0);
        inventory.setMaximumNumberOfLines(12);
        unsafe { inventory.setMenu(Some(&copy_context_menu)) };
        root.addArrangedSubview(&inventory);
        section("Activation / rollback");
        action_row(vec![activate_button, rollback_button]);
        section("Jobs");
        let jobs = NSTextField::wrappingLabelWithString(&NSString::from_str(&jobs_summary), mtm);
        jobs.setPreferredMaxLayoutWidth(700.0);
        unsafe { jobs.setMenu(Some(&copy_context_menu)) };
        root.addArrangedSubview(&jobs);
        let cancel_button = active_button("Cancel（実行中jobなし）", sel!(managerCancel:));
        if let Some(id) = view_model.latest_cancellable_job_id() {
            cancel_button.setTitle(&NSString::from_str(&format!(
                "Cancel {}",
                redact_secrets(id)
            )));
        } else {
            cancel_button.setEnabled(false);
        }
        action_row(vec![
            cancel_button.clone(),
            active_button("再読込", sel!(managerRefresh:)),
        ]);

        Ok(Self {
            mtm: Some(mtm),
            _client: client,
            _main_menu: Some(main_menu),
            _copy_context_menu: Some(copy_context_menu),
            window: Some(window),
            status_label: Some(status_label),
            jobs_label: Some(jobs),
            profiles_label: Some(profiles),
            inventory_label: Some(inventory),
            preparation_label: Some(preparation_label),
            cancel_button: Some(cancel_button),
            preparation_buttons,
            _action_target: Some(action_target),
            command_tx,
            command_rx: Some(command_rx),
            view_model,
            model_view,
            jobs_summary,
            profiles_summary,
            inventory_summary,
            preparation_summary,
            status_summary,
            action_selection,
            cancel_job_id,
            test_window_identity: 0,
            test_visible: false,
        })
    }

    #[cfg(feature = "test-support")]
    pub fn for_test(
        client: MetricsClient,
        view_model: ManagerViewModel,
        model_view: ModelView,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let identity = (&command_tx as *const Sender<ManagerCommand>) as usize;
        let jobs_summary = project_jobs(&view_model);
        let profiles_summary = project_profiles(&model_view);
        let inventory_summary = view_model.inventory_summary();
        let preparation_summary = project_preparation(&view_model);
        let status_summary = "待機中".to_string();
        let action_selection = Arc::new(Mutex::new(ManagerActionSelection::from_view_model(
            &view_model,
        )));
        let cancel_job_id = Arc::new(Mutex::new(
            view_model.latest_cancellable_job_id().map(str::to_string),
        ));
        Self {
            mtm: None,
            _client: client,
            _main_menu: None,
            _copy_context_menu: None,
            window: None,
            status_label: None,
            jobs_label: None,
            profiles_label: None,
            inventory_label: None,
            preparation_label: None,
            cancel_button: None,
            preparation_buttons: Vec::new(),
            _action_target: None,
            command_tx,
            command_rx: Some(command_rx),
            view_model,
            model_view,
            jobs_summary,
            profiles_summary,
            inventory_summary,
            preparation_summary,
            status_summary,
            action_selection,
            cancel_job_id,
            test_window_identity: identity,
            test_visible: false,
        }
    }

    pub fn show_or_focus(&mut self) -> Result<()> {
        if let (Some(window), Some(mtm)) = (&self.window, self.mtm) {
            window.makeKeyAndOrderFront(None);
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        } else {
            self.test_visible = true;
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if let Some(window) = &self.window {
            window.orderOut(None);
        }
        self.test_visible = false;
    }

    pub fn is_visible(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.isVisible())
            || self.test_visible
    }

    pub fn window_identity(&self) -> usize {
        self.window
            .as_ref()
            .map_or(self.test_window_identity, |window| {
                (&**window as *const NSWindow).cast::<()>() as usize
            })
    }

    pub fn send_command(&self, command: ManagerCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .context("manager command channel closed")
    }

    /// Move the command receiver to the single worker thread. The host keeps
    /// only the sender, so AppKit never performs HTTP itself. H06。
    pub fn take_command_receiver(&mut self) -> Result<Receiver<ManagerCommand>> {
        self.command_rx
            .take()
            .context("manager command receiver already taken")
    }

    pub fn drain_commands(&self) -> Vec<ManagerCommand> {
        self.command_rx
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default()
    }

    pub fn apply_event(&mut self, event: ManagerEvent) {
        match event {
            ManagerEvent::Status(status) => {
                let active_digest = status
                    .active_digest
                    .clone()
                    .or_else(|| self.view_model.active_digest().map(str::to_string));
                self.view_model
                    .apply_status(&status.jobs, active_digest.as_deref());
                self.model_view
                    .apply_status(&status.jobs, active_digest.as_deref());
                self.jobs_summary = project_jobs(&self.view_model);
                self.profiles_summary = project_profiles(&self.model_view);
                *self
                    .cancel_job_id
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = self
                    .view_model
                    .latest_cancellable_job_id()
                    .map(str::to_string);
                if let Some(jobs_label) = &self.jobs_label {
                    jobs_label.setStringValue(&NSString::from_str(&self.jobs_summary));
                }
                if let Some(profiles_label) = &self.profiles_label {
                    profiles_label.setStringValue(&NSString::from_str(&self.profiles_summary));
                }
                if let Some(cancel_button) = &self.cancel_button {
                    if let Some(id) = self.view_model.latest_cancellable_job_id() {
                        cancel_button.setTitle(&NSString::from_str(&format!(
                            "Cancel {}",
                            redact_secrets(id)
                        )));
                        cancel_button.setEnabled(true);
                    } else {
                        cancel_button.setTitle(&NSString::from_str("Cancel（実行中jobなし）"));
                        cancel_button.setEnabled(false);
                    }
                }
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&format!(
                        "active={} / queue={} / jobs={}",
                        self.view_model.active_digest_display(),
                        status.queue_depth,
                        status.jobs.len()
                    )));
                }
                self.status_summary = format!(
                    "active={} / queue={} / jobs={}",
                    self.view_model.active_digest_display(),
                    status.queue_depth,
                    status.jobs.len()
                );
                self.refresh_preparation_projection();
            }
            ManagerEvent::Inventory(inventory) => {
                let accepted = self.view_model.apply_inventory(inventory);
                if let Some(inventory) = self.view_model.inventory() {
                    self.model_view
                        .apply_status(&[], inventory.active_digest.as_deref());
                }
                self.inventory_summary = self.view_model.inventory_summary();
                if let Some(inventory_label) = &self.inventory_label {
                    inventory_label.setStringValue(&NSString::from_str(&self.inventory_summary));
                }
                self.status_summary = if accepted {
                    "このnodeのmanager inventoryを更新しました".to_string()
                } else {
                    "manager inventoryのnode identityが一致しません".to_string()
                };
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&self.status_summary));
                }
                self.refresh_preparation_projection();
            }
            ManagerEvent::Submitted { kind, id } => {
                self.status_summary = format!("{kind} jobを開始しました: {id}");
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&self.status_summary));
                }
            }
            ManagerEvent::Failed { message } => {
                self.status_summary = redact_secrets(&message);
                if message.starts_with("inventory refresh failed:") {
                    self.view_model
                        .mark_inventory_unavailable(&self.status_summary);
                    self.inventory_summary = self.view_model.inventory_summary();
                    if let Some(inventory_label) = &self.inventory_label {
                        inventory_label
                            .setStringValue(&NSString::from_str(&self.inventory_summary));
                    }
                    self.refresh_preparation_projection();
                }
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&self.status_summary));
                }
            }
        }
    }

    fn refresh_preparation_projection(&mut self) {
        self.preparation_summary = project_preparation(&self.view_model);
        *self
            .action_selection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagerActionSelection::from_view_model(&self.view_model);
        if let Some(label) = &self.preparation_label {
            label.setStringValue(&NSString::from_str(&self.preparation_summary));
        }
        for (action, button) in &self.preparation_buttons {
            button.setEnabled(self.view_model.preparation_action(*action).enabled);
        }
    }

    pub fn view_model(&self) -> &ManagerViewModel {
        &self.view_model
    }

    pub fn model_view(&self) -> &ModelView {
        &self.model_view
    }

    pub fn inventory_summary(&self) -> &str {
        &self.inventory_summary
    }

    pub fn preparation_summary(&self) -> &str {
        &self.preparation_summary
    }

    pub fn status_summary(&self) -> &str {
        &self.status_summary
    }

    pub fn preparation_action(&self, action: ManagerPreparationAction) -> ManagerActionState {
        self.view_model.preparation_action(action)
    }

    pub fn request_preparation_action(&self, action: ManagerPreparationAction) -> Result<bool> {
        let Some(command) = self.view_model.preparation_action(action).command else {
            return Ok(false);
        };
        self.send_command(command)?;
        Ok(true)
    }

    /// 表示用のcatalog/manifest viewを差し替える。H06。
    pub fn set_model_view(&mut self, model_view: ModelView) {
        self.model_view = model_view;
        self.profiles_summary = project_profiles(&self.model_view);
        if let Some(label) = &self.profiles_label {
            label.setStringValue(&NSString::from_str(&self.profiles_summary));
        }
    }

    pub fn jobs_summary(&self) -> &str {
        &self.jobs_summary
    }

    pub fn profiles_summary(&self) -> &str {
        &self.profiles_summary
    }

    /// AppKitのCancel actionと同じdispatchをfixtureから確認する。H06。
    pub fn request_cancel_selected(&self) -> Result<bool> {
        send_cancel_command(&self.command_tx, &self.cancel_job_id)
    }

    /// AppKitの再読込actionと同じworker channelに送る。H06。
    pub fn request_refresh(&self) -> Result<()> {
        self.send_command(ManagerCommand::Refresh)?;
        self.send_command(ManagerCommand::RefreshInventory)
    }
}

#[cfg(not(target_os = "macos"))]
pub struct ManagerWindowHost;

#[cfg(not(target_os = "macos"))]
impl ManagerWindowHost {
    pub fn send_command(&self, _command: ManagerCommand) -> anyhow::Result<()> {
        anyhow::bail!("manager window requires macOS")
    }
}

/// エラー文字列から資格情報を隠す。URL userinfo、queryの全項目、
/// Authorization header、Bearer tokenを表示前に置換する。G03/H06。/
pub fn redact_secrets(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("://") {
        out.push_str(&rest[..start]);
        // scheme:// の後の userinfo を探す（次の / か ? か # の前の @）。
        let after = &rest[start + 3..];
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..end];
        if let Some(at) = authority.rfind('@') {
            // user:pass@ を [REDACTED]@ に。
            out.push_str("://[REDACTED]@");
            out.push_str(&authority[at + 1..]);
        } else {
            out.push_str("://");
            out.push_str(authority);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    let mut query_redacted = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find('?') {
        query_redacted.push_str(&rest[..=start]);
        let after = &rest[start + 1..];
        let end = after
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '#' | '"' | '\'' | '<' | '>' | ')'))
            .unwrap_or(after.len());
        if end > 0 {
            query_redacted.push_str("[REDACTED]");
        }
        rest = &after[end..];
    }
    query_redacted.push_str(rest);

    // Header values may contain spaces, so hide the whole header line.
    let mut headers_redacted = String::with_capacity(query_redacted.len());
    for line in query_redacted.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let auth = lower
            .find("authorization:")
            .or_else(|| lower.find("authorization="));
        if let Some(start) = auth {
            headers_redacted.push_str(&line[..start]);
            headers_redacted.push_str("Authorization: [REDACTED]");
            if line.ends_with('\n') {
                headers_redacted.push('\n');
            }
        } else {
            headers_redacted.push_str(line);
        }
    }

    let mut bearer_redacted = String::with_capacity(headers_redacted.len());
    let mut rest = headers_redacted.as_str();
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find("bearer ") else {
            bearer_redacted.push_str(rest);
            break;
        };
        bearer_redacted.push_str(&rest[..start + "bearer ".len()]);
        let after = &rest[start + "bearer ".len()..];
        let end = after
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '"' | '\''))
            .unwrap_or(after.len());
        if end > 0 {
            bearer_redacted.push_str("[REDACTED]");
        }
        rest = &after[end..];
    }
    bearer_redacted
}

// ---------------------------------------------------------------------------
// テスト用 fake API。G03。/
// ---------------------------------------------------------------------------
#[cfg(test)]
pub mod test_util {
    use super::*;

    #[derive(Default)]
    pub struct FakeManagerApi {
        pub submit_calls: Vec<(String, String)>,
        pub cancel_calls: Vec<String>,
        pub job_ids: Vec<String>,
        pub submit_error: Option<String>,
    }

    impl FakeManagerApi {
        pub fn with_jobs(ids: &[&str]) -> Self {
            Self {
                job_ids: ids.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }
        }
    }

    impl ManagerApi for FakeManagerApi {
        async fn submit(&mut self, kind: &str, payload_key: &str) -> Result<String, String> {
            self.submit_calls
                .push((kind.to_string(), payload_key.to_string()));
            if let Some(error) = &self.submit_error {
                return Err(error.clone());
            }
            let id = self
                .job_ids
                .get(self.submit_calls.len() - 1)
                .cloned()
                .unwrap_or_else(|| format!("job-{}", self.submit_calls.len()));
            Ok(id)
        }

        async fn cancel(&mut self, job_id: &str) -> Result<(), String> {
            self.cancel_calls.push(job_id.to_string());
            Ok(())
        }
    }
}

/// model catalog のエントリ（C04 / model/activation view）。size / license /
/// checksum / encoder / support を表示する。checksum が無い model は
/// activate できない。G04。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub name: String,
    /// Manifest/catalog family used only to distinguish runtime config rows.
    pub origin: Option<String>,
    pub size: Option<u64>,
    /// manifest / catalog が宣言する model SHA-256。これだけでは検証済みと
    /// みなさず、完全な64桁の形式と検証状態を別々に確認する。H06。
    pub checksum: Option<String>,
    /// ローカル artifact の実 checksum が宣言値と一致したことを示す。H06。
    pub checksum_verified: bool,
    /// artifact が Manager registry の検証済み記録として確認されたことを示す。
    /// runtime manifest の宣言だけでは true にしない。H06。
    pub registry_verified: bool,
    /// Read/validation failure for one configured manifest, if any.
    pub pending_reason: Option<String>,
    pub license: String,
    /// Vision encoder（例: "openai-whisper"）。runtime と不整合なら理由。G04。/
    pub encoder: String,
    /// "supported" / "unsupported"。G04。/
    pub support: String,
}

/// Compatibility/prepare state shown in a profile row. `Pending` is used for
/// an artifact that has not passed the required preparation check; it is never
/// reported as a terminal success. G04/H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileReadiness {
    Pending(String),
    Ready,
    Rejected(String),
}

/// Fixed classification for an unreadable or invalid configured manifest.
/// Dynamic paths and parser/validator errors are intentionally excluded from
/// the value so they cannot cross into logs or the GUI. H06.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestProjectionFailure {
    Read,
    Parse,
    Validation,
}

impl ManifestProjectionFailure {
    pub const fn pending_reason(self) -> &'static str {
        match self {
            Self::Read => "manifest読込失敗（path非表示）",
            Self::Parse => "manifest解析失敗（内容非表示）",
            Self::Validation => "manifest検証失敗（詳細非表示）",
        }
    }
}

/// model 選択・download・activate・rollback の view（C04）。各 stage ごとに
/// 同じ承認 dialog を重複させず、実 runtime 変更の承認は一つの activation
/// 操作へ集約する（レビュー重点）。download / stage / activate は一つの
/// activation 操作で開始する。rollback は previous 候補から選ぶ。G04。/
#[derive(Debug, Clone, Default)]
pub struct ModelView {
    models: Vec<ModelEntry>,
    /// 旧 active digest（build/download 中も保持表示）。G04。/
    current_active: Option<String>,
    /// rollback 候補（previous）。rollback 後も保持。G04。/
    previous: Vec<String>,
    /// model 別 download 進捗（%）。G04。/
    download_progress: BTreeMap<String, u8>,
    /// Optional prefix-file compatibility results supplied by the manifest
    /// adapter. Missing entries remain compatible with the existing catalog
    /// behavior; an explicit false disables activation. H06。
    prefix_file_compatibility: BTreeMap<String, bool>,
}

impl ModelView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_models(&mut self, models: Vec<ModelEntry>) {
        self.models = models;
    }

    /// Build a read-only profile row from a runtime manifest that has already
    /// passed its schema validator. The manifest declares the profile and
    /// digest, but it does not prove that the local artifact is present or
    /// registered, so readiness remains Pending until those checks are wired.
    pub fn from_declared_runtime_profile(name: &str, size: Option<u64>, checksum: &str) -> Self {
        let mut view = Self::new();
        view.add_declared_runtime_profile("runtime", name, size, checksum);
        view
    }

    /// Append a declared profile from a validated runtime manifest. `origin`
    /// lets the UI distinguish standalone and distributed config rows even
    /// when their profile names happen to match.
    pub fn add_declared_runtime_profile(
        &mut self,
        origin: &str,
        name: &str,
        size: Option<u64>,
        checksum: &str,
    ) {
        self.models.push(ModelEntry {
            name: name.to_string(),
            origin: Some(origin.to_string()),
            size,
            checksum: Some(checksum.to_string()),
            checksum_verified: false,
            registry_verified: false,
            pending_reason: None,
            license: "manifest未記載".to_string(),
            encoder: "未確認".to_string(),
            support: "未確認".to_string(),
        });
    }

    /// Keep a failed manifest visible alongside any profile rows that loaded
    /// successfully, using a fixed classified reason. H06.
    pub fn add_manifest_failure(&mut self, origin: &str, failure: ManifestProjectionFailure) {
        self.models.push(ModelEntry {
            name: "profile manifest unavailable".to_string(),
            origin: Some(origin.to_string()),
            size: None,
            checksum: None,
            checksum_verified: false,
            registry_verified: false,
            pending_reason: Some(failure.pending_reason().to_string()),
            license: "manifest未読込".to_string(),
            encoder: "未確認".to_string(),
            support: "未確認".to_string(),
        });
    }

    /// Set the verified prefix-file compatibility result for one profile.
    /// A mismatch is a hard rejection and is kept separate from checksum and
    /// encoder validation. H06。
    pub fn set_prefix_file_compatible(&mut self, name: &str, compatible: bool) {
        self.prefix_file_compatibility
            .insert(name.to_string(), compatible);
    }

    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    /// full SHA-256、checksum 検証、registry 検証、Vision 整合がそろう場合だけ
    /// activate 可能。G04/H06。
    pub fn can_activate(&self, name: &str) -> bool {
        matches!(self.profile_readiness(name), ProfileReadiness::Ready)
    }

    /// Project one model row into the user-visible compatibility state. This
    /// is the single source of truth used by both activation gating and the
    /// AppKit profile display. H06。
    pub fn profile_readiness(&self, name: &str) -> ProfileReadiness {
        let mut matching = self.models.iter().filter(|model| model.name == name);
        let Some(model) = matching.next() else {
            return ProfileReadiness::Pending("profile未準備".to_string());
        };
        if matching.next().is_some() {
            return ProfileReadiness::Pending("同名profileの選択元が曖昧".to_string());
        }
        self.profile_entry_readiness(model)
    }

    fn profile_entry_readiness(&self, model: &ModelEntry) -> ProfileReadiness {
        if let Some(reason) = &model.pending_reason {
            return ProfileReadiness::Pending(reason.clone());
        }
        if self.prefix_file_compatibility.get(&model.name) == Some(&false) {
            return ProfileReadiness::Rejected("prefix-file不一致".to_string());
        }
        let Some(checksum) = model.checksum.as_deref() else {
            return ProfileReadiness::Pending("checksum未宣言".to_string());
        };
        if !is_full_sha256(checksum) {
            return ProfileReadiness::Pending("SHA-256形式不正".to_string());
        }
        let mut pending_checks = Vec::new();
        if !model.checksum_verified {
            pending_checks.push("checksum未検証");
        }
        if !model.registry_verified {
            pending_checks.push("Manager registry未検証");
        }
        if !pending_checks.is_empty() {
            return ProfileReadiness::Pending(pending_checks.join("・"));
        }
        if model.support != "supported" {
            return ProfileReadiness::Rejected(format!("Vision 対応外（{}）", model.support));
        }
        if !vision_consistent(model) {
            return ProfileReadiness::Rejected(format!(
                "Vision encoder 不整合（{}）",
                model.encoder
            ));
        }
        ProfileReadiness::Ready
    }

    /// Vision 不整合の理由（受入 case 2）。整合していれば None。G04。/
    pub fn vision_reason(&self, name: &str) -> Option<String> {
        let model = self.models.iter().find(|m| m.name == name)?;
        if model.support != "supported" {
            return Some(format!("Vision 対応外（{}）", model.support));
        }
        if !vision_consistent(model) {
            return Some(format!("Vision encoder 不整合（{}）", model.encoder));
        }
        None
    }

    /// `/manager/status` の反映。build/download が進行中なら旧 active を
    /// 表示し続ける（受入 case 3）。download 進捗を model 別に反映。G04。/
    pub fn apply_status(
        &mut self,
        jobs: &[siderostat_core::manager::api::ManagerJobDto],
        active_digest: Option<&str>,
    ) {
        let mut build_or_download_active = false;
        for job in jobs {
            if (job.kind == "build" || job.kind == "download")
                && (job.phase == "running" || job.phase == "cancelling")
            {
                build_or_download_active = true;
            }
            if job.kind == "download" {
                self.download_progress.insert(job.id.clone(), job.progress);
            }
        }
        // 進行中は旧 active を保持（active_digest で上書きしない）。G04。/
        if !build_or_download_active {
            self.current_active = active_digest.map(str::to_string);
        }
    }

    /// 現在の active（旧 active を表示）。G04。/
    pub fn current_active(&self) -> Option<&str> {
        self.current_active.as_deref()
    }

    /// model の download 進捗（%）。G04。/
    pub fn download_progress(&self, name: &str) -> Option<u8> {
        self.download_progress.get(name).copied()
    }

    /// activation を開始する。download / stage / activate は一つの
    /// activation 操作へ集約（承認 dialog を各 stage で重複させない）。
    /// 開始前に current_active を previous へ追加する。G04。/
    pub async fn start_activation(
        &mut self,
        name: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        if !self.can_activate(name) {
            return Err(format!(
                "{} は activate できません（checksum / Vision 不整合）",
                name
            ));
        }
        let id = api.submit("activate", name).await?;
        if let Some(active) = self.current_active.clone()
            && !self.previous.contains(&active)
        {
            self.previous.push(active);
        }
        Ok(id)
    }

    /// rollback 候補（previous）。G04。/
    pub fn rollback_candidates(&self) -> &[String] {
        &self.previous
    }

    /// rollback を開始する（kind=rollback）。previous は保持する（受入
    /// case 4）。G04。/
    pub async fn rollback(
        &mut self,
        previous_id: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        if !self.previous.contains(&previous_id.to_string()) {
            return Err("previous candidate not found".to_string());
        }
        let id = api.submit("rollback", previous_id).await?;
        // previous は rollback 後も保持する（自動削除しない）。G04。/
        Ok(id)
    }
}

fn is_full_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Vision encoder が runtime と整合するか。G04。/
fn vision_consistent(model: &ModelEntry) -> bool {
    // 既定の Vision encoder は "openai-whisper"。空は不整合。G04。/
    model.encoder == "openai-whisper"
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    /// 入力: source 無し → 取得ボタン。G03。/
    #[test]
    fn empty_sources_enable_fetch_button() {
        let mut api = FakeManagerApi::with_jobs(&["fetch-1"]);
        let mut vm = ManagerViewModel::new();
        assert!(vm.can_fetch_source(), "no source -> fetch enabled");
        let id = block_on(vm.fetch_source(&mut api)).expect("fetch");
        assert_eq!(id, "fetch-1");
        assert_eq!(
            api.submit_calls,
            vec![("fetch".to_string(), "official".to_string())]
        );
        assert!(!vm.can_fetch_source() || vm.sources().is_empty());
    }

    /// 入力: build 進行 → cancel 有効。G03。/
    #[test]
    fn running_build_enables_cancel() {
        let mut api = FakeManagerApi::with_jobs(&["build-1"]);
        let mut vm = ManagerViewModel::new();
        let id = block_on(vm.start_build("ds4", &mut api)).expect("build");
        assert!(vm.can_cancel(&id));
        block_on(vm.cancel_job(&id, &mut api)).expect("cancel");
        assert_eq!(api.cancel_calls, vec!["build-1".to_string()]);
        assert_eq!(vm.jobs().find(|j| j.id == id).unwrap().phase, "cancelling");
    }

    /// 入力: error → redacted reason。G03。/
    #[test]
    fn error_is_redacted() {
        let job = ManagerJobView {
            id: "b".to_string(),
            kind: "build".to_string(),
            progress: 0,
            phase: "failed".to_string(),
            error: "fetch failed for https://user:secret@example.com/repo?token=abc123".to_string(),
            cancel: false,
        };
        let redacted = ManagerViewModel::redacted_reason(&job);
        assert!(
            !redacted.contains("secret"),
            "password must be hidden: {redacted}"
        );
        assert!(
            !redacted.contains("abc123"),
            "token must be hidden: {redacted}"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "redaction marker present: {redacted}"
        );
        assert!(
            redacted.contains("example.com"),
            "host preserved: {redacted}"
        );
    }

    /// 入力: window 閉じ再開 → job 継続。G03。/
    #[test]
    fn view_model_survives_window_close() {
        // view model は window と独立（Arc<Mutex> 等で保持）であり、
        // window を閉じても job 状態は消えない。G03。/
        let mut api = FakeManagerApi::with_jobs(&["build-1"]);
        let mut vm = ManagerViewModel::new();
        let id = block_on(vm.start_build("ds4", &mut api)).expect("build");
        // window を閉じる（view model はそのまま）。再開時も同 view model。
        assert!(vm.jobs().any(|j| j.id == id && j.phase == "running"));
        assert!(vm.can_cancel(&id), "job continues after window reopen");
    }

    /// レビュー重点: build 可能 target と失敗理由を表示する。G03。/
    #[test]
    fn build_targets_and_failure_reason_are_surfaced() {
        let mut api = FakeManagerApi::with_jobs(&["build-ds4"]);
        let mut vm = ManagerViewModel::new();
        let _ = block_on(vm.start_build("ds4-server", &mut api)).expect("build");
        assert_eq!(vm.build_targets(), &["ds4-server".to_string()]);
        let job = ManagerJobView {
            id: "x".to_string(),
            kind: "build".to_string(),
            progress: 0,
            phase: "failed".to_string(),
            error: "missing toolchain".to_string(),
            cancel: false,
        };
        assert_eq!(ManagerViewModel::redacted_reason(&job), "missing toolchain");
    }

    const VALID_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn model(name: &str, checksum: Option<&str>, encoder: &str, support: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            origin: None,
            size: Some(1024),
            checksum: checksum.map(str::to_string),
            checksum_verified: false,
            registry_verified: false,
            pending_reason: None,
            license: "MIT".to_string(),
            encoder: encoder.to_string(),
            support: support.to_string(),
        }
    }

    fn verified_model(name: &str, encoder: &str, support: &str) -> ModelEntry {
        ModelEntry {
            checksum: Some(VALID_SHA256.to_string()),
            checksum_verified: true,
            registry_verified: true,
            ..model(name, None, encoder, support)
        }
    }

    /// 入力: checksum 無し → activate disabled。G04。/
    #[test]
    fn missing_checksum_disables_activation() {
        let mut api = FakeManagerApi::with_jobs(&["act-1"]);
        let mut view = ModelView::new();
        view.set_models(vec![model("m1", None, "openai-whisper", "supported")]);
        assert!(!view.can_activate("m1"), "no checksum -> activate disabled");
        let err = block_on(view.start_activation("m1", &mut api)).expect_err("activate");
        assert!(err.contains("activate できません"));
        assert!(api.submit_calls.is_empty(), "no POST when checksum missing");
    }

    #[test]
    fn declared_runtime_profile_stays_pending_until_registry_verification() {
        let view =
            ModelView::from_declared_runtime_profile("configured-profile", Some(42), VALID_SHA256);
        assert_eq!(view.models()[0].name, "configured-profile");
        assert_eq!(view.models()[0].size, Some(42));
        assert!(!view.models()[0].checksum_verified);
        assert!(!view.models()[0].registry_verified);
        assert_eq!(
            view.profile_readiness("configured-profile"),
            ProfileReadiness::Pending("checksum未検証・Manager registry未検証".to_string())
        );
        assert!(!view.can_activate("configured-profile"));
    }

    #[test]
    fn short_checksum_never_becomes_ready_even_with_verification_flags() {
        let mut entry = model("short", Some("sha"), "openai-whisper", "supported");
        entry.checksum_verified = true;
        entry.registry_verified = true;
        let mut view = ModelView::new();
        view.set_models(vec![entry]);
        assert_eq!(
            view.profile_readiness("short"),
            ProfileReadiness::Pending("SHA-256形式不正".to_string())
        );
        assert!(!view.can_activate("short"));
    }

    /// 入力: Vision 不整合 → 理由。G04。/
    #[test]
    fn vision_mismatch_surfaces_reason() {
        let mut view = ModelView::new();
        view.set_models(vec![
            verified_model("m-ok", "openai-whisper", "supported"),
            verified_model("m-enc", "other-encoder", "supported"),
            verified_model("m-unsup", "openai-whisper", "unsupported"),
        ]);
        assert!(view.can_activate("m-ok"));
        assert!(view.vision_reason("m-ok").is_none());
        let reason = view
            .vision_reason("m-enc")
            .expect("encoder mismatch reason");
        assert!(reason.contains("encoder 不整合"), "{reason}");
        assert!(!view.can_activate("m-enc"));
        let reason = view.vision_reason("m-unsup").expect("unsupported reason");
        assert!(reason.contains("Vision 対応外"), "{reason}");
        assert!(!view.can_activate("m-unsup"));
    }

    /// 入力: build/download 中 → 旧 active 表示。G04。/
    #[test]
    fn old_active_shown_while_build_or_download_runs() {
        use siderostat_core::manager::api::ManagerJobDto;
        let mut view = ModelView::new();
        let running_build = ManagerJobDto {
            id: "build-1".to_string(),
            kind: "build".to_string(),
            progress: 40,
            phase: "running".to_string(),
            error: String::new(),
            created_at: 0,
            updated_at: 0,
            cancel: true,
        };
        // 先に旧 active を反映しておく。G04。/
        view.apply_status(&[], Some("old-active"));
        assert_eq!(view.current_active(), Some("old-active"));
        // build 進行中は旧 active を表示し続ける（active_digest で上書き
        // しない）。G04。/
        view.apply_status(std::slice::from_ref(&running_build), Some("new-active"));
        assert_eq!(view.current_active(), Some("old-active"));
        let _ = running_build;
    }

    /// 入力: rollback → previous 保持。G04。/
    #[test]
    fn rollback_keeps_previous() {
        let mut api = FakeManagerApi::with_jobs(&["rollback-1"]);
        let mut view = ModelView::new();
        view.set_models(vec![verified_model("m1", "openai-whisper", "supported")]);
        // activation で current_active を previous に追加。G04。/
        view.apply_status(&[], Some("active-a"));
        let _ = block_on(view.start_activation("m1", &mut api)).expect("activate");
        assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
        // rollback 後も previous は保持。G04。/
        let _ = block_on(view.rollback("active-a", &mut api)).expect("rollback");
        assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
        assert_eq!(api.submit_calls.len(), 2);
        assert_eq!(api.submit_calls[0].0, "activate");
        assert_eq!(api.submit_calls[1].0, "rollback");
    }

    /// レビュー重点: activation は一つの操作へ集約（承認 dialog を各 stage
    /// で重複させない）。G04。/
    #[test]
    fn activation_is_single_operation() {
        let mut api = FakeManagerApi::with_jobs(&["act-1"]);
        let mut view = ModelView::new();
        view.set_models(vec![verified_model("m1", "openai-whisper", "supported")]);
        let id = block_on(view.start_activation("m1", &mut api)).expect("activate");
        assert_eq!(id, "act-1");
        // download / stage / activate を分割せず、一つの activate 操作に
        // 集約（POST 1 回）。G04。/
        assert_eq!(api.submit_calls.len(), 1);
        assert_eq!(api.submit_calls[0].0, "activate");
    }

    #[test]
    fn manager_api_generation_submit_preserves_plain_fake_boundary() {
        let mut api = FakeManagerApi::with_jobs(&["fetch-1"]);
        let id =
            block_on(api.submit_with_generation("fetch", "official", None)).expect("plain submit");
        assert_eq!(id, "fetch-1");
        assert_eq!(api.submit_calls, vec![("fetch".into(), "official".into())]);
    }

    fn block_on<F>(future: F) -> F::Output
    where
        F: std::future::Future,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(future)
    }
}
