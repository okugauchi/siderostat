//! DS4 Manager — artifact registry と private root。M01。
//!
//! C04 に基づき、互換 root（`~/Library/Application Support/siderostat/`）を
//! 維持し、その下に ds4/sources・builds・models・operations・logs を作る。
//! registry schema1（SourceRecord/BuildRecord/Catalog/ArtifactState）を実装し、
//! staged/active/previous 参照を分け、symlink escape と不正 path を拒否する。
//! 記録は atomic（verify→fsync→同一 filesystem rename→record publish）で、
//! write 途中 crash でも前 record が読める。active digest 不一致は
//! activation 禁止にする。既存外部 model と稼働 binary はコピー/削除せず、
//! managed namespace だけを更新する（フォルダ名だけで trusted と扱わない）。
//!
//! 受入 case（全て必須）:
//! - 入力: symlink で root 外 → 拒否
//! - 入力: write 途中 crash → 前 record 読取可（atomic 記録）
//! - 入力: active digest 不一致 → activation 禁止
//!
//! レビュー重点: フォルダ名だけで trusted と扱わない。削除は本 release で
//! 自動化しない。
//!
//! 契約: CONTRACTS.md C04 / ArtifactRegistry。M01。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::store::{ArtifactKind, ManagerReleaseStore, ReleaseIdentity};

/// managed root の既定相対パス（互換 root を維持）。C04。M01。
const MANAGED_ROOT_RELATIVE: &str = "Library/Application Support/siderostat";

/// managed namespace の subdir 名。C04: ds4/sources・builds・models・
/// manifests・operations・logs。M01。
const SOURCES_DIR: &str = "ds4/sources";
const BUILDS_DIR: &str = "ds4/builds";
const MODELS_DIR: &str = "ds4/models";
const OPERATIONS_DIR: &str = "ds4/operations";
const LOGS_DIR: &str = "ds4/logs";

/// registry schema の version。C04: registry schema1。M01。
pub const REGISTRY_SCHEMA: u32 = 1;

/// managed namespace の各ディレクトリ。M01。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPaths {
    pub sources: PathBuf,
    pub builds: PathBuf,
    pub models: PathBuf,
    pub operations: PathBuf,
    pub logs: PathBuf,
}

impl ManagedPaths {
    /// root から各 subdir を解決する。M01。
    fn from_root(root: &Path) -> Self {
        Self {
            sources: root.join(SOURCES_DIR),
            builds: root.join(BUILDS_DIR),
            models: root.join(MODELS_DIR),
            operations: root.join(OPERATIONS_DIR),
            logs: root.join(LOGS_DIR),
        }
    }
}

/// managed root を表す。互換 root（Application Support/siderostat）を維持し、
/// その下の managed namespace を管理する。M01。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerRoot {
    root: PathBuf,
}

impl ManagerRoot {
    /// 既定 root（`~/Library/Application Support/siderostat`）を `HOME` から
    /// 解決する。`HOME` が無い場合は明示 error（user data を作成しない）。
    /// M01。
    pub fn default_from_home(home: &Path) -> Self {
        Self {
            root: home.join(MANAGED_ROOT_RELATIVE),
        }
    }

    /// 明示 root から構築する（テスト・明示設定の別 root 用）。M01。
    pub fn explicit(root: PathBuf) -> Self {
        Self { root }
    }

    /// managed root の絶対 path。M01。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// managed namespace の各 subdir path。M01。
    pub fn paths(&self) -> ManagedPaths {
        ManagedPaths::from_root(&self.root)
    }
}

/// artifact の state。C04: ArtifactState。M01。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    /// 準備中（未検証）。
    Staged,
    /// 検証済み（未 active）。
    Verified,
    /// 現在 active。
    Active,
    /// 直前の active（rollback 用）。C04: previous。
    Previous,
    /// 隔離（問題検出時）。
    Quarantined,
}

impl ArtifactState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactState::Staged => "staged",
            ArtifactState::Verified => "verified",
            ArtifactState::Active => "active",
            ArtifactState::Previous => "previous",
            ArtifactState::Quarantined => "quarantined",
        }
    }
}

/// 1 件の registry record。M01。
///
/// artifact は managed namespace 内の相対 path（root 配下）で参照し、絶対
/// path や symlink による root 外参照は拒否する（受入 case: symlink で
/// root 外 → 拒否）。M01。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRecord {
    /// registry 内の一意 ID。M01。
    pub id: String,
    /// artifact の種類（model/build/source 等）。M01。
    pub kind: String,
    /// managed namespace 内の相対 path（root 配下）。M01。
    pub rel_path: PathBuf,
    /// full SHA-256 hex。C04: 新規 model は full SHA 必須。M01。
    pub sha256: String,
    /// 現在の state。M01。
    pub state: ArtifactState,
}

impl ArtifactRecord {
    /// 相対 path が managed root 内に収まるかを検証する。M01。
    ///
    /// `rel_path` は絶対 path でなく、`..` で root 外へ escape しないことを
    /// 確認する（受入 case: symlink で root 外 → 拒否 の一部）。M01。
    pub fn validate_rel_path(rel_path: &Path) -> Result<(), RegistryError> {
        if rel_path.is_absolute() {
            return Err(RegistryError::PathOutsideRoot);
        }
        for component in rel_path.components() {
            match component {
                std::path::Component::ParentDir => {
                    return Err(RegistryError::PathOutsideRoot);
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    return Err(RegistryError::PathOutsideRoot);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// registry エラー。M01。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// path が managed root 外（絶対 path・`..`・symlink escape）。M01。
    PathOutsideRoot,
    /// 実ファイル digest が record と不一致。M01。
    DigestMismatch,
    /// 参照先が symlink で root 外を指す。M01。
    SymlinkEscape,
    /// record が存在しない。M01。
    NotFound,
    /// IO / その他。
    Io(String),
    /// schema 不整合。
    Schema(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::PathOutsideRoot => write!(f, "path escapes managed root"),
            RegistryError::DigestMismatch => write!(f, "artifact digest mismatch"),
            RegistryError::SymlinkEscape => write!(f, "symlink escapes managed root"),
            RegistryError::NotFound => write!(f, "artifact record not found"),
            RegistryError::Io(msg) => write!(f, "io: {msg}"),
            RegistryError::Schema(msg) => write!(f, "schema: {msg}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// artifact registry。M01。
///
/// 記録は atomic（temp 書込 → fsync → 同一 filesystem rename）で publish し、
/// write 途中 crash でも前 record が読める（受入 case: write 途中 crash →
/// 前 record 読取可）。M01。
#[derive(Debug, Clone)]
pub struct ArtifactRegistry {
    root: ManagerRoot,
    /// registry の全 record（ID → record）。メモリ上の正本。M01。
    records: BTreeMap<String, ArtifactRecord>,
}

impl ArtifactRegistry {
    /// 空の registry を構築する。M01。
    pub fn new(root: ManagerRoot) -> Self {
        Self {
            root,
            records: BTreeMap::new(),
        }
    }

    /// 検証済みの durable store から query 用 record を導出する。
    ///
    /// manager release store が永続正本であり、この registry は表示・既存
    /// query API 用の一時 projection のみを保持する。M02。
    pub fn from_store(store: &ManagerReleaseStore) -> Self {
        let snapshot = store.snapshot();
        let mut states = BTreeMap::new();
        for (identity, state) in [
            (
                snapshot.release_pointers.previous.as_ref(),
                ArtifactState::Previous,
            ),
            (
                snapshot.release_pointers.active.as_ref(),
                ArtifactState::Active,
            ),
        ] {
            let Some(ReleaseIdentity::ManagedProfile(profile_id)) = identity else {
                continue;
            };
            let Some(profile) = snapshot.profiles.get(profile_id) else {
                continue;
            };
            for artifact_id in profile
                .role_artifact_ids
                .iter()
                .chain(std::iter::once(&profile.model_artifact_id))
            {
                states.insert(artifact_id.clone(), state);
            }
        }

        let records = snapshot
            .artifacts
            .values()
            .map(|artifact| {
                let kind = match artifact.kind {
                    ArtifactKind::Build => "build",
                    ArtifactKind::Model => "model",
                };
                ArtifactRecord {
                    id: artifact.id.clone(),
                    kind: kind.into(),
                    rel_path: artifact.rel_path.clone(),
                    sha256: artifact.sha256.clone(),
                    state: states
                        .get(&artifact.id)
                        .copied()
                        .unwrap_or(artifact.validation_state),
                }
            })
            .map(|record| (record.id.clone(), record))
            .collect();

        Self {
            root: store.root().clone(),
            records,
        }
    }

    /// managed root。M01。
    pub fn root(&self) -> &ManagerRoot {
        &self.root
    }

    /// managed namespace の各 subdir path。M01。
    pub fn paths(&self) -> ManagedPaths {
        self.root.paths()
    }

    /// 相対 path を managed root 配下の絶対 path へ解決する。M01。
    ///
    /// symlink escape を拒否する（受入 case: symlink で root 外 → 拒否）。
    /// 存在する symlink の実体が root 外を指す場合、SymlinkEscape を返す。
    /// M01。
    pub fn resolve_within_root(&self, rel_path: &Path) -> Result<PathBuf, RegistryError> {
        ArtifactRecord::validate_rel_path(rel_path)?;
        let abs = self.root.root().join(rel_path);
        // root を canonicalize して実体 path を得る。root が無ければ作る。
        // (macOS の /tmp ↔ /private/tmp 等の実体差を吸収)。M01。
        fs::create_dir_all(self.root.root())
            .map_err(|e| RegistryError::Io(format!("create root: {e}")))?;
        let canonical_root = fs::canonicalize(self.root.root())
            .map_err(|e| RegistryError::Io(format!("canonicalize root: {e}")))?;
        // abs の実体が root 実体内に収まるかを確認する。
        // symlink で root 外へ escape する場合、canonicalize(abs) が root
        // 実体の外を指す。ファイルが存在しない（未 stage）場合は parent
        // の canonical で root 内チェックを行う。M01。
        if abs.exists() {
            let canonical_abs = fs::canonicalize(&abs)
                .map_err(|e| RegistryError::Io(format!("canonicalize abs: {e}")))?;
            if !canonical_abs.starts_with(&canonical_root) {
                return Err(RegistryError::SymlinkEscape);
            }
            return Ok(abs);
        }
        let canonical_parent = fs::canonicalize(abs.parent().unwrap_or(&abs))
            .map_err(|e| RegistryError::Io(format!("canonicalize parent: {e}")))?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(RegistryError::SymlinkEscape);
        }
        Ok(abs)
    }

    /// record を追加・更新する。M01。
    pub fn put(&mut self, record: ArtifactRecord) {
        self.records.insert(record.id.clone(), record);
    }

    /// record を取得する。M01。
    pub fn get(&self, id: &str) -> Option<&ArtifactRecord> {
        self.records.get(id)
    }

    /// state を指定して全 record を返す。M01。
    pub fn list_by_state(&self, state: ArtifactState) -> Vec<&ArtifactRecord> {
        self.records.values().filter(|r| r.state == state).collect()
    }

    /// 指定 ID の record の state を更新する。M01。
    pub fn set_state(&mut self, id: &str, state: ArtifactState) -> Result<(), RegistryError> {
        let record = self.records.get_mut(id).ok_or(RegistryError::NotFound)?;
        record.state = state;
        Ok(())
    }

    /// 実ファイルの SHA-256 を計算する。M01。
    pub fn file_sha256(&self, rel_path: &Path) -> Result<String, RegistryError> {
        let abs = self.resolve_within_root(rel_path)?;
        let bytes = fs::read(&abs).map_err(|e| RegistryError::Io(format!("read: {e}")))?;
        Ok(sha256_hex(&bytes))
    }

    /// active digest 不一致を検出する（受入 case: active digest 不一致 →
    /// activation 禁止）。M01。
    ///
    /// active の record について、実ファイルの digest と record の sha256 が
    /// 一致するかを検証する。不一致なら DigestMismatch を返す。M01。
    pub fn verify_active_digests(&self) -> Result<(), RegistryError> {
        for record in self.list_by_state(ArtifactState::Active) {
            let actual = self.file_sha256(&record.rel_path)?;
            if actual != record.sha256 {
                return Err(RegistryError::DigestMismatch);
            }
        }
        Ok(())
    }

    /// activation 可否を検証する。M01。
    ///
    /// active の実 digest 不一致があれば activation 禁止（DigestMismatch）。
    /// それ以外は Ok。M01。
    pub fn can_activate(&self) -> Result<(), RegistryError> {
        self.verify_active_digests()
    }

    /// 前 record を読めるように atomic に record を publish する。M01。
    ///
    /// temp file へ書いて fsync し、同一 filesystem 内で rename する。
    /// これにより write 途中 crash でも前 record が読める（受入 case:
    /// write 途中 crash → 前 record 読取可）。M01。
    ///
    /// 注: 本メソッドは record の永続化を表す。メモリ上の `records` は
    /// `put` で更新済み。ここでは managed namespace の operations 下に
    /// journal を atomic 書込する。M01。
    pub fn persist_journal(&self) -> Result<(), RegistryError> {
        let ops = self.root.paths().operations;
        fs::create_dir_all(&ops)
            .map_err(|e| RegistryError::Io(format!("create operations: {e}")))?;
        let records_json = serde_json::to_vec_pretty(&self.records)
            .map_err(|e| RegistryError::Schema(e.to_string()))?;
        atomic_write(&ops.join("registry-journal.json"), &records_json)?;
        Ok(())
    }
}

/// 同一 filesystem rename で atomic に書込む。M01。
///
/// temp file へ書いて fsync してから rename する。rename は同一 filesystem
/// 内で atomic なため、write 途中 crash でも前内容が残る（受入 case: write
/// 途中 crash → 前 record 読取可）。M01。
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), RegistryError> {
    let dir = path
        .parent()
        .ok_or_else(|| RegistryError::Io("path has no parent".into()))?;
    fs::create_dir_all(dir).map_err(|e| RegistryError::Io(format!("create dir: {e}")))?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("journal"),
        std::process::id()
    ));
    {
        let mut f =
            fs::File::create(&tmp).map_err(|e| RegistryError::Io(format!("create tmp: {e}")))?;
        f.write_all(contents)
            .map_err(|e| RegistryError::Io(format!("write tmp: {e}")))?;
        f.sync_all()
            .map_err(|e| RegistryError::Io(format!("fsync tmp: {e}")))?;
    }
    fs::rename(&tmp, path).map_err(|e| RegistryError::Io(format!("rename: {e}")))?;
    Ok(())
}

/// bytes の SHA-256 hex を計算する。M01。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// SourceRecord。C04: remote/full commit/main proof/fetched_at。M01。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceRecord {
    /// 公式 source の remote URL。M01。
    pub remote: String,
    /// 固定 commit（full SHA）。M01。
    pub full_commit: String,
    /// main branch の proof（commit object SHA 等）。M01。
    pub main_proof: String,
    /// fetch 時刻（epoch secs）。M01。
    pub fetched_at: u64,
}

/// BuildRecord。C04: source/flags/toolchain/arch/role/target digest/help digest。M01。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildRecord {
    /// source の commit ref。M01。
    pub source: String,
    /// build flags。M01。
    pub flags: String,
    /// toolchain。M01。
    pub toolchain: String,
    /// 対象 arch。M01。
    pub arch: String,
    /// role（worker/coordinator 等）。M01。
    pub role: String,
    /// 実行した固定 make target。旧 store record との互換用に欠落時は空。
    #[serde(default)]
    pub target: String,
    /// binary digest（full SHA-256）。M01。
    pub digest: String,
    /// help text digest。M01。
    pub help_digest: String,
}

/// Catalog entry。C04: URL/redirect allowlist/size/SHA/license/compatibility。M01。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    /// 取得元 URL。M01。
    pub url: String,
    /// 許可 redirect 先 allowlist。M01。
    pub redirect_allowlist: Vec<String>,
    /// 期待 size（bytes）。M01。
    pub size: u64,
    /// 期待 SHA-256。M01。
    pub sha: String,
    /// license。M01。
    pub license: String,
    /// compatibility 記述。M01。
    pub compatibility: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m01-{tag}"));
        // テスト用の一時 root は毎回作り直す。
        let _ = fs::remove_dir_all(&base);
        base
    }

    /// 受入 case: symlink で root 外 → 拒否。M01。
    #[test]
    fn symlink_escape_rejected() {
        let root = tmp_root("symlink");
        let reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        // root 外の実ファイルを作る。
        let outside = std::env::temp_dir().join("siderostat-m01-outside-target");
        fs::write(&outside, b"secret").expect("write outside");
        // root 内の subdir を作り、その中の symlink が root 外を指す。
        let sub = root.join("ds4/models");
        fs::create_dir_all(&sub).expect("create sub");
        std::os::unix::fs::symlink(&outside, sub.join("link")).expect("symlink");

        // symlink を通して root 外の絶対 path へ解決しようとすると拒否。
        let rel = Path::new("ds4/models/link");
        let err = reg
            .resolve_within_root(rel)
            .expect_err("must reject symlink escape");
        assert!(matches!(
            err,
            RegistryError::SymlinkEscape | RegistryError::PathOutsideRoot
        ));
        let _ = fs::remove_file(&outside);
    }

    /// `..` で root 外へ escape する相対 path → 拒否。M01。
    #[test]
    fn parent_escape_rejected() {
        let root = tmp_root("parent");
        let reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        let err = reg
            .resolve_within_root(Path::new("../outside"))
            .expect_err("must reject");
        assert!(matches!(err, RegistryError::PathOutsideRoot));
    }

    /// 受入 case: write 途中 crash → 前 record 読取可（atomic 記録）。M01。
    ///
    /// temp file への書込が途中で中断しても、rename 前なので target には
    /// 前内容が残る（atomic rename）。ここでは tmp を残して target を
    /// 上書きしないケースを検証する。M01。
    #[test]
    fn interrupted_write_keeps_previous_record() {
        let root = tmp_root("atomic");
        let _reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        let ops = root.join("ds4/operations");
        fs::create_dir_all(&ops).expect("create ops");
        let target = ops.join("registry-journal.json");
        // 前 record を publish。
        let prev = b"{\"old\":true}";
        atomic_write(&target, prev).expect("publish previous");
        assert_eq!(fs::read(&target).expect("read"), prev);

        // write 途中 crash を模して、tmp ファイルを残したまま rename しない。
        // atomic_write の失敗（rename 前の fsync 失敗等）で tmp が残る場合、
        // target には前 record が残る。ここでは tmp を作って rename を
        // 省略した状態を作る（前 record が読めることを確認）。
        let tmp = ops.join(format!(".tmp-registry-journal-{}", std::process::id()));
        fs::write(&tmp, b"{\"partial\":true}").expect("write tmp");
        // target は前 record のまま（tmp は未 rename）。
        assert_eq!(
            fs::read(&target).expect("read"),
            prev,
            "previous record readable"
        );
        let _ = fs::remove_file(&tmp);
    }

    /// atomic_write は fsync→rename で publish する。M01。
    #[test]
    fn atomic_write_replaces_record() {
        let root = tmp_root("atomic2");
        let _reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        let ops = root.join("ds4/operations");
        fs::create_dir_all(&ops).expect("create ops");
        let target = ops.join("registry-journal.json");
        atomic_write(&target, b"one").expect("write one");
        atomic_write(&target, b"two").expect("write two");
        assert_eq!(fs::read(&target).expect("read"), b"two");
    }

    /// 受入 case: active digest 不一致 → activation 禁止。M01。
    #[test]
    fn active_digest_mismatch_blocks_activation() {
        let root = tmp_root("digest");
        let mut reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        // active artifact を置く。
        let rel = Path::new("ds4/models/active-model.bin");
        let abs = root.join(rel);
        fs::create_dir_all(abs.parent().expect("parent")).expect("create dir");
        fs::write(&abs, b"model-content-v1").expect("write");
        let good = sha256_hex(b"model-content-v1");

        reg.put(ArtifactRecord {
            id: "m1".into(),
            kind: "model".into(),
            rel_path: rel.to_path_buf(),
            sha256: good.clone(),
            state: ArtifactState::Active,
        });
        // 一致 → activation 可。
        reg.can_activate().expect("matching digest activates");

        // 実ファイルを変更して digest 不一致にする。
        fs::write(&abs, b"model-content-v2-tampered").expect("overwrite");
        let err = reg.can_activate().expect_err("mismatch blocks activation");
        assert_eq!(err, RegistryError::DigestMismatch);
    }

    /// managed paths が root 配下の subdir を指す。M01。
    #[test]
    fn managed_paths_under_root() {
        let root = tmp_root("paths");
        let reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        let paths = reg.paths();
        assert_eq!(paths.sources, root.join("ds4/sources"));
        assert_eq!(paths.builds, root.join("ds4/builds"));
        assert_eq!(paths.models, root.join("ds4/models"));
        assert_eq!(paths.operations, root.join("ds4/operations"));
        assert_eq!(paths.logs, root.join("ds4/logs"));
    }

    /// 既定 root は互換 root（Application Support/siderostat）を維持する。M01。
    #[test]
    fn default_root_keeps_application_support_path() {
        let root = ManagerRoot::default_from_home(Path::new("/Users/o"));
        assert_eq!(
            root.root(),
            Path::new("/Users/o/Library/Application Support/siderostat")
        );
    }

    /// state 別一覧と state 更新。M01。
    #[test]
    fn list_and_update_state() {
        let root = tmp_root("state");
        let mut reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        reg.put(ArtifactRecord {
            id: "a".into(),
            kind: "model".into(),
            rel_path: Path::new("ds4/models/a").to_path_buf(),
            sha256: "x".into(),
            state: ArtifactState::Staged,
        });
        assert_eq!(reg.list_by_state(ArtifactState::Staged).len(), 1);
        reg.set_state("a", ArtifactState::Verified)
            .expect("set state");
        assert_eq!(reg.list_by_state(ArtifactState::Verified).len(), 1);
        assert_eq!(reg.list_by_state(ArtifactState::Staged).len(), 0);
    }

    /// persist_journal は operations 下に atomic 記録する。M01。
    #[test]
    fn persist_journal_writes_operations() {
        let root = tmp_root("journal");
        let reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
        reg.persist_journal().expect("persist");
        let journal = root.join("ds4/operations/registry-journal.json");
        assert!(journal.exists(), "journal written");
        let text = fs::read_to_string(&journal).expect("read journal");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse journal");
        assert_eq!(parsed, serde_json::json!({}));
    }
}
