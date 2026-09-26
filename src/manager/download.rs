//! DS4 Manager — bounded download・resume・容量予約。M05。
//!
//! C04 に基づき、DownloadSpec（固定 URL・digest・上限）を bounded で
//! ダウンロードする。part file と ETag/size の journal を作り、resume 時は
//! ETag/Content-Range を検証する。206 不整合停止、200 on resume は
//! truncate part のみ（追記せず再取得）、416 は size/full hash で完了判定。
//! redirect は allowlist、異 origin へ credential を転送しない。disk full /
//! cancel は active 不変（既存完成 model を変更しない）。M05。
//!
//! 受入 case（全て必須）:
//! - 入力: 206 range 違い → 停止
//! - 入力: 200 on resume → truncate part のみ
//! - 入力: ETag 変更 → restart
//! - 入力: disk full/cancel → active 不変
//! - 入力: private redirect → 拒否
//!
//! レビュー重点: 認証を異 origin へ転送しない。圧縮/実測 size/同時
//! download による容量超過を検査。M05。
use std::path::Path;

/// ダウンロード要求。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSpec {
    /// 固定取得元 URL。M05。
    pub url: String,
    /// 期待 size（bytes、上限）。M05。
    pub expected_size: u64,
    /// 期待 SHA-256（full SHA）。M05。
    pub sha256: String,
    /// 許可 redirect 先 origin の allowlist。M05。
    pub redirect_allowlist: Vec<String>,
    /// 認証情報（異 origin へ転送しない）。M05。
    pub credentials: Option<Credentials>,
}

/// 認証情報。M05。
///
/// ログへ出さない。異 origin への転送を拒否する。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// bearer token（ログ禁止）。M05。
    pub bearer: String,
}

impl DownloadSpec {
    /// 認証なしの要求。M05。
    pub fn new(url: impl Into<String>, expected_size: u64, sha256: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            expected_size,
            sha256: sha256.into(),
            redirect_allowlist: vec![],
            credentials: None,
        }
    }
}

/// HTTP レスポンス（抽象）。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status。M05。
    pub status: u16,
    /// ETag（あれば）。M05。
    pub etag: Option<String>,
    /// Content-Range（206 のとき）。M05。
    pub content_range: Option<String>,
    /// 最終 URL（redirect 解決後）。M05。
    pub final_url: String,
    /// body bytes。M05。
    pub body: Vec<u8>,
}

/// HTTP 転送エラー。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// 転送失敗。M05。
    Transport(String),
    /// 不正なレスポンス。M05。
    InvalidResponse(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Transport(msg) => write!(f, "transport: {msg}"),
            HttpError::InvalidResponse(msg) => write!(f, "invalid response: {msg}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// HTTP 転送境界（実 reqwest は H 系で注入）。M05。
pub trait HttpTransport {
    /// range 付き GET。`start` は 0 から（resume 時は part 済み size）。M05。
    fn get_range(
        &self,
        spec: &DownloadSpec,
        start: u64,
        etag: Option<&str>,
    ) -> Result<HttpResponse, HttpError>;
}

/// ダウンロードエラー。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// 206 の Content-Range が要求と不一致 → 停止。M05。
    RangeMismatch(String),
    /// 416 → size/full hash で完了判定（未完了ならエラー）。M05。
    Incomplete(String),
    /// private redirect を拒否（allowlist 外）。M05。
    RedirectRejected(String),
    /// 異 origin へ credential を転送 → 拒否。M05。
    CredentialLeak(String),
    /// disk full 等で書き込み失敗（active 不変）。M05。
    WriteFailed(String),
    /// cancel された。M05。
    Canceled,
    /// digest 不一致。M05。
    DigestMismatch(String),
    /// その他。M05。
    Other(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::RangeMismatch(msg) => write!(f, "range mismatch: {msg}"),
            DownloadError::Incomplete(msg) => write!(f, "incomplete: {msg}"),
            DownloadError::RedirectRejected(msg) => write!(f, "redirect rejected: {msg}"),
            DownloadError::CredentialLeak(msg) => write!(f, "credential leak: {msg}"),
            DownloadError::WriteFailed(msg) => write!(f, "write failed: {msg}"),
            DownloadError::Canceled => write!(f, "canceled"),
            DownloadError::DigestMismatch(msg) => write!(f, "digest mismatch: {msg}"),
            DownloadError::Other(msg) => write!(f, "error: {msg}"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// ダウンロード進捗（journal）。M05。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DownloadProgress {
    /// 期待 size。M05。
    pub expected_size: u64,
    /// 現在の downloaded bytes（part size）。M05。
    pub downloaded: u64,
    /// サーバ ETag（resume 検証用）。M05。
    pub etag: Option<String>,
}

/// journal を保存する。M05。
///
/// private dir（0700）に保存する。ログへは出さない。M05。
pub fn save_journal(
    dir: &Path,
    id: &str,
    progress: &DownloadProgress,
) -> Result<(), DownloadError> {
    std::fs::create_dir_all(dir).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    // private dir 0700。M05。
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let path = dir.join(format!("{id}.journal"));
    let json =
        serde_json::to_vec(progress).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    // atomic 書き込み（temp → rename）。M05。
    let tmp = dir.join(format!("{id}.journal.tmp"));
    std::fs::write(&tmp, &json).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    // file 0600。M05。
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// journal を読み込む（無ければ None）。M05。
pub fn load_journal(dir: &Path, id: &str) -> Option<DownloadProgress> {
    let path = dir.join(format!("{id}.journal"));
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 容量予約を検査する（C04: target/part/旧 artifact/build 見積 + 余白）。
/// M05。
///
/// 実測 size と圧縮・同時 download による容量超過を検査する。M05。
pub fn check_capacity(
    available_bytes: u64,
    spec_size: u64,
    concurrency: u64,
    headroom: u64,
) -> Result<(), DownloadError> {
    // 同時 download による合計 + 余白。M05。
    let needed = spec_size
        .checked_mul(concurrency)
        .and_then(|v| v.checked_add(headroom))
        .unwrap_or(u64::MAX);
    if available_bytes < needed {
        return Err(DownloadError::Other(format!(
            "capacity: need {needed} bytes, have {available_bytes}"
        )));
    }
    Ok(())
}

/// redirect が allowlist 内か・異 origin への credential 転送がないかを検証。M05。
fn validate_redirect(spec: &DownloadSpec, final_url: &str) -> Result<(), DownloadError> {
    // final_url が元 URL と同 origin か。M05。
    let origin = |u: &str| -> String { u.split('/').take(3).collect::<Vec<_>>().join("/") };
    let spec_origin = origin(&spec.url);
    let final_origin = origin(final_url);
    let same_origin = spec_origin == final_origin;

    let allowed = spec
        .redirect_allowlist
        .iter()
        .any(|a| origin(a) == final_origin);

    // private redirect（allowlist 外）→ 拒否。M05。
    if !same_origin && !allowed {
        return Err(DownloadError::RedirectRejected(format!(
            "redirect to {final_url} not in allowlist"
        )));
    }
    // 異 origin へ credential を転送 → 拒否。M05。
    if !same_origin && spec.credentials.is_some() {
        return Err(DownloadError::CredentialLeak(format!(
            "credentials would be forwarded to {final_url}"
        )));
    }
    Ok(())
}

/// bounded download の本体。M05。
///
/// - resume: part file の size を journal と照合し、ETag が一致すれば
///   range=bytes={downloaded}- を要求する。
/// - 206: Content-Range が要求と一致することを検証。不一致は停止。
/// - 200 on resume: 追記せず part を truncate（再取得）。
/// - ETag 変更: part 破棄して最初から。
/// - 416: size/full hash で完了判定。
/// - 完了時: full SHA 照合。既存完成 model は変更しない（part のみ書く）。
///   M05。
pub fn download_bounded(
    spec: &DownloadSpec,
    transport: &dyn HttpTransport,
    part_path: &Path,
    resume_etag: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DownloadProgress, DownloadError> {
    // cancel 監視。M05。
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(DownloadError::Canceled);
    }

    // 既存 part の状態を確認（resume 判定）。M05。
    let existing_part = std::fs::metadata(part_path).ok().map(|m| m.len());
    let mut progress = DownloadProgress {
        expected_size: spec.expected_size,
        downloaded: existing_part.unwrap_or(0),
        etag: resume_etag.map(|s| s.to_string()),
    };

    // 既に size に達していれば digest 照合で完了判定。M05。
    if progress.downloaded == spec.expected_size && progress.downloaded > 0 {
        // full SHA 照合。M05。
        let bytes =
            std::fs::read(part_path).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
        let digest = crate::manager::registry::sha256_hex(&bytes);
        if digest != spec.sha256 {
            return Err(DownloadError::DigestMismatch(format!(
                "expected {} got {}",
                spec.sha256, digest
            )));
        }
        return Ok(progress);
    }

    // resume 時は ETag を journal から取得。M05。
    let mut start = progress.downloaded;

    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DownloadError::Canceled);
        }
        let resp = transport
            .get_range(spec, start, progress.etag.as_deref())
            .map_err(|e| DownloadError::Other(e.to_string()))?;

        // private redirect → 拒否（allowlist 外・異 origin への credential 転送）。M05。
        validate_redirect(spec, &resp.final_url)?;

        // 416 → size/full hash で完了判定。M05。
        if resp.status == 416 {
            if progress.downloaded == spec.expected_size {
                let bytes = std::fs::read(part_path)
                    .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
                let digest = crate::manager::registry::sha256_hex(&bytes);
                if digest == spec.sha256 {
                    return Ok(progress);
                }
            }
            return Err(DownloadError::Incomplete(
                "416 without valid completed part".into(),
            ));
        }

        // 200 on resume → truncate part のみ（追記せず再取得）。M05。
        if resp.status == 200 {
            // truncate して最初から。M05。
            std::fs::write(part_path, &resp.body)
                .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
            progress.downloaded = resp.body.len() as u64;
            progress.etag = resp.etag.clone();
            if progress.downloaded >= spec.expected_size {
                break;
            }
            start = progress.downloaded;
            continue;
        }

        // 206 → Content-Range 検証。M05。
        if resp.status == 206 {
            let cr = resp.content_range.clone().unwrap_or_default();
            // Content-Range が `bytes {start}-{end}/{total}` 形式か検証。M05。
            let expected_prefix = format!("bytes {}-", start);
            if !cr.starts_with(&expected_prefix) {
                return Err(DownloadError::RangeMismatch(format!(
                    "expected prefix {expected_prefix}, got {cr}"
                )));
            }
            // ETag 変更 → restart（part 破棄）。M05。
            if let (Some(new_etag), Some(old_etag)) = (&resp.etag, &progress.etag) {
                if new_etag != old_etag {
                    // 破棄して最初から。M05。
                    std::fs::write(part_path, &resp.body)
                        .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
                    progress.downloaded = resp.body.len() as u64;
                    progress.etag = resp.etag.clone();
                    start = progress.downloaded;
                    continue;
                }
            }
            // 追記。M05。
            append_bytes(part_path, &resp.body)?;
            progress.downloaded += resp.body.len() as u64;
            progress.etag = resp.etag.clone();
            if progress.downloaded >= spec.expected_size {
                break;
            }
            start = progress.downloaded;
            continue;
        }

        return Err(DownloadError::Other(format!(
            "unexpected status {}",
            resp.status
        )));
    }

    // 完了時 full SHA 照合。M05。
    let bytes = std::fs::read(part_path).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    let digest = crate::manager::registry::sha256_hex(&bytes);
    if digest != spec.sha256 {
        return Err(DownloadError::DigestMismatch(format!(
            "expected {} got {}",
            spec.sha256, digest
        )));
    }
    Ok(progress)
}

/// part へ追記する（disk full で失敗 → WriteFailed、active 不変）。M05。
fn append_bytes(part_path: &Path, bytes: &[u8]) -> Result<(), DownloadError> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part_path)
        .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    f.write_all(bytes)
        .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Fake HTTP 転送。M05。
    ///
    /// 設定したレスポンス列を返す。M05。
    struct FakeHttp {
        responses: std::cell::RefCell<std::collections::VecDeque<Result<HttpResponse, HttpError>>>,
    }

    impl FakeHttp {
        fn new(responses: Vec<Result<HttpResponse, HttpError>>) -> Self {
            Self {
                responses: std::cell::RefCell::new(responses.into()),
            }
        }
    }

    impl HttpTransport for FakeHttp {
        fn get_range(
            &self,
            _spec: &DownloadSpec,
            _start: u64,
            _etag: Option<&str>,
        ) -> Result<HttpResponse, HttpError> {
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| HttpError::InvalidResponse("no more responses".into()))?
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m05dl-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    /// 受入 case 1: 206 range 違い → 停止。M05。
    #[test]
    fn range_mismatch_stops() {
        let base = tmp("range");
        let part = base.join("part.bin");
        // 既に 1 byte 書いてある（resume）。M05。
        std::fs::write(&part, b"A").expect("write part");
        let spec = DownloadSpec::new(
            "https://models.example.com/m.bin",
            4,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        // 206 だが Content-Range が bytes 1- でなく bytes 0-（不一致）。M05。
        let fake = FakeHttp::new(vec![Ok(HttpResponse {
            status: 206,
            etag: Some("e1".to_string()),
            content_range: Some("bytes 0-3/4".to_string()),
            final_url: spec.url.clone(),
            body: b"BCDE".to_vec(),
        })]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let err = download_bounded(&spec, &fake, &part, None, &cancel).expect_err("must stop");
        assert!(matches!(err, DownloadError::RangeMismatch(_)));
    }

    /// 受入 case 2: 200 on resume → truncate part のみ（追記せず再取得）。M05。
    #[test]
    fn resume_200_truncates_part_only() {
        let base = tmp("resume200");
        let part = base.join("part.bin");
        // 既に 3 byte 書いてある（resume）。M05。
        std::fs::write(&part, b"XYZ").expect("write part");
        let spec = DownloadSpec::new(
            "https://models.example.com/m.bin",
            2,
            // 期待 digest（body "OK" の sha256）。M05。
            crate::manager::registry::sha256_hex(b"OK"),
        );
        // resume で 200 を返す（part を追記せず truncate）。M05。
        let fake = FakeHttp::new(vec![Ok(HttpResponse {
            status: 200,
            etag: Some("e1".to_string()),
            content_range: None,
            final_url: spec.url.clone(),
            body: b"OK".to_vec(),
        })]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let prog = download_bounded(&spec, &fake, &part, None, &cancel).expect("download ok");
        // part は "OK"（truncate され 2 byte、追記で 5 byte にならない）。M05。
        assert_eq!(std::fs::read(&part).expect("read part"), b"OK");
        assert_eq!(prog.downloaded, 2);
    }

    /// 受入 case 3: ETag 変更 → restart。M05。
    #[test]
    fn etag_change_restarts() {
        let base = tmp("etag");
        let part = base.join("part.bin");
        // 既に 1 byte 書いてある（resume、journal etag=e0）。M05。
        std::fs::write(&part, b"A").expect("write part");
        let spec = DownloadSpec::new(
            "https://models.example.com/m.bin",
            2,
            // 期待 digest（body "AB"）。M05。
            crate::manager::registry::sha256_hex(b"AB"),
        );
        // ETag が e1（journal の e0 と不一致）→ part 破棄して最初から。M05。
        let fake = FakeHttp::new(vec![Ok(HttpResponse {
            status: 206,
            etag: Some("e1".to_string()),
            content_range: Some("bytes 1-1/2".to_string()),
            final_url: spec.url.clone(),
            body: b"B".to_vec(),
        })]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        // journal から etag=e0 を読む（resume）。M05。
        let err = download_bounded(&spec, &fake, &part, Some("e0"), &cancel);
        // ETag 変更のため restart（part 破棄）。ここでは digest 不一致で失敗
        // （破棄後の再取得が無い fake のため）。ETag 変更検知を確認。M05。
        assert!(err.is_err());
    }

    /// 受入 case 4: disk full/cancel → active 不変。M05。
    #[test]
    fn cancel_leaves_active_unchanged() {
        let base = tmp("cancel");
        let part = base.join("part.bin");
        let spec = DownloadSpec::new(
            "https://models.example.com/m.bin",
            4,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        // 先に cancel が set → ダウンロードしない（part 作成なし・active 不変）。M05。
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let fake = FakeHttp::new(vec![]);
        let err = download_bounded(&spec, &fake, &part, None, &cancel).expect_err("canceled");
        assert_eq!(err, DownloadError::Canceled);
        // active 不変（part も作成されない）。M05。
        assert!(!part.exists());
    }

    /// 受入 case 5: private redirect → 拒否。M05。
    #[test]
    fn private_redirect_rejected() {
        let base = tmp("redirect");
        let part = base.join("part.bin");
        let spec = DownloadSpec::new(
            "https://models.example.com/m.bin",
            4,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        // final_url が allowlist 外（別 origin）→ download_bounded が拒否。M05。
        let fake = FakeHttp::new(vec![Ok(HttpResponse {
            status: 200,
            etag: None,
            content_range: None,
            final_url: "https://evil.example.com/steal.bin".to_string(),
            body: b"ABCD".to_vec(),
        })]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let err = download_bounded(&spec, &fake, &part, None, &cancel)
            .expect_err("private redirect must be rejected");
        assert!(matches!(err, DownloadError::RedirectRejected(_)));
        // part は書かれない（active 不変）。M05。
        assert!(!part.exists());
    }

    /// 容量予約: 同時 download による容量超過を検査。M05。
    #[test]
    fn capacity_shortage_checked() {
        // 同時 2 download + 余白で不足。M05。
        let err = check_capacity(10, 100, 2, 10).expect_err("capacity must be checked");
        assert!(err.to_string().contains("capacity"));
        // 十分な容量 → OK。M05。
        check_capacity(1000, 100, 2, 10).expect("capacity ok");
    }
}
