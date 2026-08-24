#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{collections::BTreeSet, fs, path::PathBuf};
    use uuid::Uuid;

    fn temporary_root() -> PathBuf {
        std::env::temp_dir().join(format!("siderostat-diagnostics-{}", Uuid::new_v4()))
    }

    #[test]
    fn snapshot_schema_contains_only_redacted_contract_fields() {
        let snapshot = fixture(Uuid::new_v4(), 1_700_000_000_000);
        let value = serde_json::to_value(&snapshot).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "admission",
                "captured_at_millis",
                "children",
                "cluster",
                "control_session",
                "node_id",
                "os",
                "process",
                "progress",
                "recovery_id",
                "schema_version",
                "network",
            ])
        );
        assert_eq!(
            value["schema_version"],
            json!(DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION)
        );
        assert_eq!(value["cluster"]["generation"], 12);
        assert_eq!(value["control_session"]["generation"], 7);
        assert_eq!(value["progress"]["generation"]["token_delta"], 4);

        let encoded = serde_json::to_string(&value).unwrap();
        for forbidden in FORBIDDEN_SNAPSHOT_KEYS {
            assert!(!encoded.contains(forbidden), "forbidden field: {forbidden}");
        }
    }

    #[test]
    fn atomic_write_creates_private_snapshot_file() {
        let root = temporary_root();
        let store = DiagnosticSnapshotStore::new(&root, 8).unwrap();
        let snapshot = fixture(Uuid::new_v4(), 1_700_000_000_000);

        let path = store.write(&snapshot).unwrap();
        assert!(path.ends_with("snapshot.json"));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
        assert!(!root.join(".snapshot.json.tmp").exists());
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_atomic_write_does_not_leave_a_snapshot() {
        let root = temporary_root();
        fs::write(&root, b"not a directory").unwrap();
        let store = DiagnosticSnapshotStore::new(root.clone(), 8).unwrap();

        assert!(
            store
                .write(&fixture(Uuid::new_v4(), 1_700_000_000_000))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&root).unwrap(), "not a directory");
        fs::remove_file(root).unwrap();
    }

    #[test]
    fn retention_keeps_only_the_newest_snapshots() {
        let root = temporary_root();
        let store = DiagnosticSnapshotStore::new(&root, 2).unwrap();
        fs::create_dir_all(root.join("unmanaged")).unwrap();
        for captured_at in 1..=3 {
            store.write(&fixture(Uuid::new_v4(), captured_at)).unwrap();
        }

        let directories = fs::read_dir(&root).unwrap().count();
        assert_eq!(directories, 3);
        assert!(root.join("unmanaged").exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture(recovery_id: Uuid, captured_at_millis: u64) -> DiagnosticSnapshot {
        DiagnosticSnapshot::fixture(recovery_id, captured_at_millis)
    }
}
use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    cluster::{
        ChildDiagnostics, ChildrenDiagnostics, ClusterSnapshot, ControlMode, ControlRole,
        ControlSessionDiagnostics, DistributedControlPhase, LeaseDiagnostics, PeerDiagnostics,
        ProductionDiagnostics,
    },
    metrics::{MetricsDiagnosticSnapshot, ProgressDiagnosticSnapshot as MetricsProgressSnapshot},
    proxy::ModeAwareTargetSnapshot,
    target::ProxyTarget,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_DIAGNOSTIC_SNAPSHOT_RETENTION: usize = 8;

const SNAPSHOT_FILE_NAME: &str = "snapshot.json";
const SNAPSHOT_ROOT_RELATIVE: &str = "Library/Application Support/siderostat/recovery/snapshots";
#[cfg(test)]
const FORBIDDEN_SNAPSHOT_KEYS: &[&str] = &[
    "\"prompt\":",
    "\"response\":",
    "\"authorization\":",
    "\"api_key\":",
    "\"token\":",
    "\"cookie\":",
    "\"session_id\":",
    "\"request_id\":",
    "\"deployment_id\":",
    "\"model_digest\":",
    "\"peer_proxy_token\":",
    "\"hmac_secret\":",
    "\"signature\":",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticSnapshot {
    pub schema_version: u32,
    pub recovery_id: String,
    pub captured_at_millis: u64,
    pub node_id: String,
    pub cluster: ClusterDiagnosticSnapshot,
    pub control_session: Option<ControlDiagnosticSnapshot>,
    pub admission: AdmissionDiagnosticSnapshot,
    pub children: ChildrenDiagnosticSnapshot,
    pub process: ProcessDiagnosticSnapshot,
    pub progress: ProgressDiagnosticSnapshotGroup,
    pub network: NetworkDiagnosticSnapshot,
    pub os: OsDiagnosticSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClusterDiagnosticSnapshot {
    pub generation: u64,
    pub role: String,
    pub mode: String,
    pub state: String,
    pub target: String,
    pub target_ready: bool,
    pub local_standalone_ready: bool,
    pub last_failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlDiagnosticSnapshot {
    pub generation: u64,
    pub phase: String,
    pub role: String,
    pub peer_distributed_child_generation: Option<u64>,
    pub lease: LeaseDiagnosticSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeaseDiagnosticSnapshot {
    pub valid: bool,
    pub expires_at_millis: Option<u64>,
    pub route_scoped: bool,
    pub peer_present: bool,
    pub peer: Option<PeerDiagnosticSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerDiagnosticSnapshot {
    pub node_id: String,
    pub role: String,
    pub generation: u64,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChildrenDiagnosticSnapshot {
    pub standalone: Option<ChildDiagnosticSnapshot>,
    pub distributed_coordinator: Option<ChildDiagnosticSnapshot>,
    pub distributed_worker: Option<ChildDiagnosticSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChildDiagnosticSnapshot {
    pub pid: Option<u32>,
    pub profile: Option<String>,
    pub generation: Option<u64>,
    pub running: bool,
    pub ready: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessDiagnosticSnapshot {
    pub managed_children: u32,
    pub running_children: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionDiagnosticSnapshot {
    pub state: String,
    pub in_flight: u64,
    pub max_in_flight: u64,
    pub drain_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProgressDiagnosticSnapshot {
    pub active: bool,
    pub progress_observed: bool,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub percent: Option<f64>,
    pub cached: Option<u64>,
    pub chunk_tps: Option<f64>,
    pub avg_tps: Option<f64>,
    pub elapsed_secs: Option<f64>,
    pub age_secs: Option<f64>,
    pub token_delta: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressDiagnosticSnapshotGroup {
    pub in_flight: u64,
    pub generation_in_flight: u64,
    pub prefill: ProgressDiagnosticSnapshot,
    pub generation: ProgressDiagnosticSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkDiagnosticSnapshot {
    pub interface: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OsDiagnosticSnapshot {
    pub family: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone)]
pub struct DiagnosticSnapshotStore {
    root: PathBuf,
    retention: usize,
}

impl DiagnosticSnapshotStore {
    pub fn new(root: impl Into<PathBuf>, retention: usize) -> Result<Self> {
        ensure!(
            retention > 0,
            "diagnostic snapshot retention must be greater than zero"
        );
        Ok(Self {
            root: root.into(),
            retention,
        })
    }

    pub fn for_current_user() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Self::new(
            home.join(SNAPSHOT_ROOT_RELATIVE),
            DEFAULT_DIAGNOSTIC_SNAPSHOT_RETENTION,
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, snapshot: &DiagnosticSnapshot) -> Result<PathBuf> {
        validate_snapshot(snapshot)?;
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create diagnostic snapshot root {}", self.root.display()))?;
        set_private_directory(&self.root)?;

        let recovery_id = Uuid::parse_str(&snapshot.recovery_id)
            .context("diagnostic snapshot recovery_id must be a UUID")?;
        let directory = self.root.join(recovery_id.to_string());
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "create diagnostic snapshot directory {}",
                directory.display()
            )
        })?;
        set_private_directory(&directory)?;

        let final_path = directory.join(SNAPSHOT_FILE_NAME);
        if final_path.exists() {
            bail!(
                "diagnostic snapshot already exists: {}",
                final_path.display()
            );
        }
        let temporary_path = directory.join(format!(".snapshot-{}.tmp", Uuid::new_v4()));
        let result = self.write_atomic_file(&temporary_path, &final_path, snapshot);
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result?;
        self.prune()?;
        sync_directory(&self.root)?;
        Ok(final_path)
    }

    fn write_atomic_file(
        &self,
        temporary_path: &Path,
        final_path: &Path,
        snapshot: &DiagnosticSnapshot,
    ) -> Result<()> {
        let mut bytes =
            serde_json::to_vec_pretty(snapshot).context("serialize diagnostic snapshot")?;
        bytes.push(b'\n');
        let mut file = private_file(temporary_path)?;
        file.write_all(&bytes)
            .with_context(|| format!("write {}", temporary_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary_path.display()))?;
        drop(file);
        fs::rename(temporary_path, final_path).with_context(|| {
            format!(
                "atomically install diagnostic snapshot {}",
                final_path.display()
            )
        })?;
        sync_directory(final_path.parent().context("snapshot has no parent")?)?;
        Ok(())
    }

    fn prune(&self) -> Result<()> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("read diagnostic snapshot root {}", self.root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let snapshot_path = path.join(SNAPSHOT_FILE_NAME);
            let Ok(bytes) = fs::read(&snapshot_path) else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_slice::<DiagnosticSnapshot>(&bytes) else {
                continue;
            };
            entries.push((snapshot.captured_at_millis, snapshot.recovery_id, path));
        }
        entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        for (_, _, path) in entries.into_iter().skip(self.retention) {
            fs::remove_dir_all(&path)
                .with_context(|| format!("prune diagnostic snapshot {}", path.display()))?;
        }
        Ok(())
    }
}

impl DiagnosticSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn from_runtime(
        recovery_id: Uuid,
        captured_at_millis: u64,
        node_id: &str,
        interface: &str,
        cluster: Option<ClusterSnapshot>,
        target: ModeAwareTargetSnapshot,
        admission: AdmissionSnapshot,
        production: Option<&ProductionDiagnostics>,
        metrics: MetricsDiagnosticSnapshot,
    ) -> Self {
        let cluster_snapshot =
            cluster.unwrap_or_else(|| ClusterSnapshot::booting(crate::target::LocalRole::Unknown));
        let cluster_record = ClusterDiagnosticSnapshot {
            generation: cluster_snapshot.generation,
            role: cluster_snapshot.role.name().to_string(),
            mode: cluster_snapshot.stable_mode.name().to_string(),
            state: cluster_snapshot.state.name().to_string(),
            target: proxy_target_name(target.target).to_string(),
            target_ready: target.ready,
            local_standalone_ready: cluster_snapshot.local_standalone_ready,
            last_failure: cluster_snapshot
                .last_failure
                .map(|failure| format!("{failure:?}")),
        };
        let control_session =
            production.map(|diagnostics| control_snapshot(&diagnostics.control_session));
        let children = production
            .map(|diagnostics| children_snapshot(&diagnostics.children))
            .unwrap_or_default();
        let process = process_snapshot(&children);
        Self {
            schema_version: DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION,
            recovery_id: recovery_id.to_string(),
            captured_at_millis,
            node_id: node_id.to_string(),
            cluster: cluster_record,
            control_session,
            admission: admission_snapshot(admission),
            children,
            process,
            progress: progress_snapshot(metrics),
            network: NetworkDiagnosticSnapshot {
                interface: interface.to_string(),
            },
            os: OsDiagnosticSnapshot {
                family: std::env::consts::FAMILY.to_string(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            },
        }
    }

    #[cfg(test)]
    fn fixture(recovery_id: Uuid, captured_at_millis: u64) -> Self {
        Self {
            schema_version: DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION,
            recovery_id: recovery_id.to_string(),
            captured_at_millis,
            node_id: "node-a".to_string(),
            cluster: ClusterDiagnosticSnapshot {
                generation: 12,
                role: "coordinator".to_string(),
                mode: "distributed-layer-parallel".to_string(),
                state: "distributed-ready".to_string(),
                target: "local-standalone".to_string(),
                target_ready: true,
                local_standalone_ready: true,
                last_failure: None,
            },
            control_session: Some(ControlDiagnosticSnapshot {
                generation: 7,
                phase: "worker-ready".to_string(),
                role: "coordinator".to_string(),
                peer_distributed_child_generation: None,
                lease: LeaseDiagnosticSnapshot {
                    valid: true,
                    expires_at_millis: Some(1_700_000_001_000),
                    route_scoped: true,
                    peer_present: true,
                    peer: Some(PeerDiagnosticSnapshot {
                        node_id: "node-b".to_string(),
                        role: "worker".to_string(),
                        generation: 7,
                        mode: "distributed-layer-parallel".to_string(),
                    }),
                },
            }),
            admission: AdmissionDiagnosticSnapshot {
                state: "serving".to_string(),
                in_flight: 0,
                max_in_flight: 4,
                drain_generation: None,
            },
            children: ChildrenDiagnosticSnapshot::default(),
            process: ProcessDiagnosticSnapshot {
                managed_children: 0,
                running_children: 0,
            },
            progress: ProgressDiagnosticSnapshotGroup {
                in_flight: 0,
                generation_in_flight: 1,
                prefill: ProgressDiagnosticSnapshot::default(),
                generation: ProgressDiagnosticSnapshot {
                    active: true,
                    progress_observed: true,
                    current: Some(4),
                    total: None,
                    percent: None,
                    cached: None,
                    chunk_tps: Some(6.5),
                    avg_tps: Some(5.2),
                    elapsed_secs: Some(1.0),
                    age_secs: Some(0.5),
                    token_delta: 4,
                },
            },
            network: NetworkDiagnosticSnapshot {
                interface: "bridge0".to_string(),
            },
            os: OsDiagnosticSnapshot {
                family: "unix".to_string(),
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
            },
        }
    }
}

fn validate_snapshot(snapshot: &DiagnosticSnapshot) -> Result<()> {
    ensure!(
        snapshot.schema_version == DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION,
        "unsupported diagnostic snapshot schema version {}",
        snapshot.schema_version
    );
    Uuid::parse_str(&snapshot.recovery_id)
        .context("diagnostic snapshot recovery_id must be a UUID")?;
    Ok(())
}

fn admission_snapshot(snapshot: AdmissionSnapshot) -> AdmissionDiagnosticSnapshot {
    AdmissionDiagnosticSnapshot {
        state: admission_state_name(snapshot.state).to_string(),
        in_flight: snapshot.in_flight as u64,
        max_in_flight: snapshot.max_in_flight as u64,
        drain_generation: snapshot.drain_generation,
    }
}

fn admission_state_name(state: AdmissionState) -> &'static str {
    match state {
        AdmissionState::Serving => "serving",
        AdmissionState::Draining => "draining",
        AdmissionState::Blocked => "blocked",
    }
}

fn control_snapshot(session: &ControlSessionDiagnostics) -> ControlDiagnosticSnapshot {
    ControlDiagnosticSnapshot {
        generation: session.generation,
        phase: control_phase_name(session.phase).to_string(),
        role: control_role_name(session.role).to_string(),
        peer_distributed_child_generation: session.peer_distributed_child_generation,
        lease: lease_snapshot(&session.lease),
    }
}

fn lease_snapshot(lease: &LeaseDiagnostics) -> LeaseDiagnosticSnapshot {
    LeaseDiagnosticSnapshot {
        valid: lease.valid,
        expires_at_millis: lease.expires_at_millis,
        route_scoped: lease.route_scoped,
        peer_present: lease.peer_present,
        peer: lease.peer.as_ref().map(peer_snapshot),
    }
}

fn peer_snapshot(peer: &PeerDiagnostics) -> PeerDiagnosticSnapshot {
    PeerDiagnosticSnapshot {
        node_id: peer.node_id.clone(),
        role: control_role_name(peer.role).to_string(),
        generation: peer.generation,
        mode: control_mode_name(peer.mode).to_string(),
    }
}

fn children_snapshot(children: &ChildrenDiagnostics) -> ChildrenDiagnosticSnapshot {
    ChildrenDiagnosticSnapshot {
        standalone: children.standalone.as_ref().map(child_snapshot),
        distributed_coordinator: children
            .distributed_coordinator
            .as_ref()
            .map(child_snapshot),
        distributed_worker: children.distributed_worker.as_ref().map(child_snapshot),
    }
}

fn child_snapshot(child: &ChildDiagnostics) -> ChildDiagnosticSnapshot {
    ChildDiagnosticSnapshot {
        pid: child.pid,
        profile: child.profile.clone(),
        generation: child.generation,
        running: child.running,
        ready: child.ready,
    }
}

fn process_snapshot(children: &ChildrenDiagnosticSnapshot) -> ProcessDiagnosticSnapshot {
    let managed = [
        children.standalone.as_ref(),
        children.distributed_coordinator.as_ref(),
        children.distributed_worker.as_ref(),
    ];
    ProcessDiagnosticSnapshot {
        managed_children: managed.iter().filter(|child| child.is_some()).count() as u32,
        running_children: managed
            .iter()
            .filter(|child| child.is_some_and(|child| child.running))
            .count() as u32,
    }
}

fn progress_snapshot(metrics: MetricsDiagnosticSnapshot) -> ProgressDiagnosticSnapshotGroup {
    ProgressDiagnosticSnapshotGroup {
        in_flight: metrics.in_flight,
        generation_in_flight: metrics.generation_in_flight,
        prefill: progress_from_metrics(metrics.prefill),
        generation: progress_from_metrics(metrics.generation),
    }
}

fn progress_from_metrics(progress: MetricsProgressSnapshot) -> ProgressDiagnosticSnapshot {
    let has_progress = progress.active || progress.progress_observed;
    ProgressDiagnosticSnapshot {
        active: progress.active,
        progress_observed: progress.progress_observed,
        current: has_progress.then_some(progress.current).flatten(),
        total: progress.total.filter(|total| *total > 0),
        percent: has_progress.then(|| finite(progress.percent)).flatten(),
        cached: has_progress.then_some(progress.cached).flatten(),
        chunk_tps: has_progress.then(|| finite(progress.chunk_tps)).flatten(),
        avg_tps: has_progress.then(|| finite(progress.avg_tps)).flatten(),
        elapsed_secs: has_progress
            .then(|| finite(progress.elapsed_secs))
            .flatten(),
        age_secs: progress.age_secs.and_then(finite),
        token_delta: progress.token_delta,
    }
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn proxy_target_name(target: ProxyTarget) -> &'static str {
    match target {
        ProxyTarget::LocalStandalone => "local-standalone",
        ProxyTarget::Coordinator => "coordinator",
        ProxyTarget::Unavailable { .. } => "unavailable",
    }
}

fn control_role_name(role: ControlRole) -> &'static str {
    match role {
        ControlRole::Coordinator => "coordinator",
        ControlRole::Worker => "worker",
    }
}

fn control_mode_name(mode: ControlMode) -> &'static str {
    match mode {
        ControlMode::SoloStandalone => "solo-standalone",
        ControlMode::PairedStandalone => "paired-standalone",
        ControlMode::DistributedLayerParallel => "distributed-layer-parallel",
        ControlMode::Transitioning => "transitioning",
    }
}

fn control_phase_name(phase: DistributedControlPhase) -> &'static str {
    match phase {
        DistributedControlPhase::Unpaired => "unpaired",
        DistributedControlPhase::Paired => "paired",
        DistributedControlPhase::WorkerPreparing => "worker-preparing",
        DistributedControlPhase::WorkerReady => "worker-ready",
        DistributedControlPhase::Draining => "draining",
        DistributedControlPhase::Drained => "drained",
    }
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("create private diagnostic snapshot {}", path.display()))
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open diagnostic snapshot directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync diagnostic snapshot directory {}", path.display()))
}

pub(crate) fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX))
}
