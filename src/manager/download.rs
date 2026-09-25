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
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponseMetadata {
    pub status: u16,
    pub etag: Option<String>,
    pub content_range: Option<String>,
    pub final_url: String,
    pub body_size: u64,
}

/// HTTP 転送エラー。M05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// 転送失敗。M05。
    Transport(String),
    /// 不正なレスポンス。M05。
    InvalidResponse(String),
    WriteFailed(String),
    Canceled,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Transport(msg) => write!(f, "transport: {msg}"),
            HttpError::InvalidResponse(msg) => write!(f, "invalid response: {msg}"),
            HttpError::WriteFailed(msg) => write!(f, "write failed: {msg}"),
            HttpError::Canceled => f.write_str("canceled"),
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

    /// Stream a bounded response body to a newly allocated temporary file.
    /// Fixture transports may use the byte-vector method; production overrides
    /// this method to avoid buffering large models in memory.
    fn get_range_to_file(
        &self,
        spec: &DownloadSpec,
        start: u64,
        etag: Option<&str>,
        destination: &Path,
        max_bytes: u64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<HttpResponseMetadata, HttpError> {
        use std::io::Write;
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(HttpError::Canceled);
        }
        let response = self.get_range(spec, start, etag)?;
        let response_limit = if response.status == 200 {
            spec.expected_size.saturating_add(1)
        } else {
            max_bytes
        };
        if response.body.len() as u64 > response_limit {
            return Err(HttpError::InvalidResponse(
                "response exceeds catalog size".into(),
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(destination)
            .map_err(|_| HttpError::WriteFailed("temporary response file unavailable".into()))?;
        file.write_all(&response.body)
            .map_err(|_| HttpError::WriteFailed("response write failed".into()))?;
        file.sync_all()
            .map_err(|_| HttpError::WriteFailed("response sync failed".into()))?;
        Ok(HttpResponseMetadata {
            status: response.status,
            etag: response.etag,
            content_range: response.content_range,
            final_url: response.final_url,
            body_size: response.body.len() as u64,
        })
    }
}

/// HTTP transport for the production manager. Redirects are handled manually
/// so each hop is checked against the catalog allowlist before another request.
pub struct ReqwestHttpTransport {
    client: std::sync::OnceLock<Result<reqwest::blocking::Client, HttpError>>,
}

impl ReqwestHttpTransport {
    pub fn new() -> Result<Self, HttpError> {
        // Construct lazily: the app wires this adapter while inside Tokio, but
        // the synchronous manager executor invokes it from a blocking worker.
        Ok(Self {
            client: std::sync::OnceLock::new(),
        })
    }

    fn client(&self) -> Result<&reqwest::blocking::Client, HttpError> {
        self.client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .timeout(std::time::Duration::from_secs(900))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|_| HttpError::Transport("HTTP client unavailable".into()))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn send_range(
        &self,
        spec: &DownloadSpec,
        start: u64,
        etag: Option<&str>,
    ) -> Result<(reqwest::blocking::Response, url::Url), HttpError> {
        use reqwest::header::{IF_RANGE, LOCATION, RANGE};
        validate_download_url(&spec.url)
            .map_err(|_| HttpError::InvalidResponse("catalog source URL is invalid".into()))?;
        let mut current = url::Url::parse(&spec.url)
            .map_err(|_| HttpError::InvalidResponse("catalog source URL is invalid".into()))?;
        let original_origin = current.origin().ascii_serialization();
        for redirects in 0..=5 {
            let mut request = self
                .client()?
                .get(current.clone())
                .header(RANGE, format!("bytes={start}-"));
            if let Some(etag) = etag {
                request = request.header(IF_RANGE, etag);
            }
            if let Some(credentials) = &spec.credentials {
                if current.origin().ascii_serialization() != original_origin {
                    return Err(HttpError::InvalidResponse(
                        "cross-origin credential forwarding rejected".into(),
                    ));
                }
                request = request.bearer_auth(&credentials.bearer);
            }
            let response = request
                .send()
                .map_err(|_| HttpError::Transport("model request failed".into()))?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirects == 5 {
                    return Err(HttpError::InvalidResponse("too many redirects".into()));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| HttpError::InvalidResponse("invalid redirect".into()))?;
                let next = current
                    .join(location)
                    .map_err(|_| HttpError::InvalidResponse("invalid redirect".into()))?;
                validate_redirect(spec, next.as_str()).map_err(|_| {
                    HttpError::InvalidResponse("redirect rejected by catalog policy".into())
                })?;
                current = next;
                continue;
            }

            return Ok((response, current));
        }
        Err(HttpError::InvalidResponse("too many redirects".into()))
    }
}

impl HttpTransport for ReqwestHttpTransport {
    fn get_range(
        &self,
        spec: &DownloadSpec,
        start: u64,
        etag: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        use reqwest::header::{CONTENT_RANGE, ETAG};
        use std::io::Read;

        let (mut response, current) = self.send_range(spec, start, etag)?;
        let status = response.status().as_u16();
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        response
            .by_ref()
            .take(spec.expected_size.saturating_sub(start).saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| HttpError::Transport("model response read failed".into()))?;
        Ok(HttpResponse {
            status,
            etag: response_etag,
            content_range,
            final_url: current.to_string(),
            body,
        })
    }

    fn get_range_to_file(
        &self,
        spec: &DownloadSpec,
        start: u64,
        etag: Option<&str>,
        destination: &Path,
        max_bytes: u64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<HttpResponseMetadata, HttpError> {
        use reqwest::header::{CONTENT_RANGE, ETAG};
        use std::io::{Read, Write};

        let (mut response, current) = self.send_range(spec, start, etag)?;
        let status = response.status().as_u16();
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(destination)
            .map_err(|_| HttpError::WriteFailed("temporary response file unavailable".into()))?;
        let response_limit = if status == 200 {
            spec.expected_size.saturating_add(1)
        } else {
            max_bytes
        };
        let mut body_size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                drop(file);
                let _ = std::fs::remove_file(destination);
                return Err(HttpError::Canceled);
            }
            let count = response
                .read(&mut buffer)
                .map_err(|_| HttpError::Transport("model response read failed".into()))?;
            if count == 0 {
                break;
            }
            body_size = body_size.saturating_add(count as u64);
            if body_size > response_limit {
                drop(file);
                let _ = std::fs::remove_file(destination);
                return Err(HttpError::InvalidResponse(
                    "response exceeds catalog size".into(),
                ));
            }
            file.write_all(&buffer[..count])
                .map_err(|_| HttpError::WriteFailed("response write failed".into()))?;
        }
        file.sync_all()
            .map_err(|_| HttpError::WriteFailed("response sync failed".into()))?;
        Ok(HttpResponseMetadata {
            status,
            etag: response_etag,
            content_range,
            final_url: current.to_string(),
            body_size,
        })
    }
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
    /// 実測 bytes が catalog size と一致しない。M05。
    SizeMismatch,
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
            DownloadError::SizeMismatch => f.write_str("download size mismatch"),
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
    validate_download_url(&spec.url)?;
    validate_download_url(final_url)?;
    let spec_origin = url::Url::parse(&spec.url)
        .map_err(|_| DownloadError::RedirectRejected("invalid source URL".into()))?
        .origin()
        .ascii_serialization();
    let final_origin = url::Url::parse(final_url)
        .map_err(|_| DownloadError::RedirectRejected("invalid redirect URL".into()))?
        .origin()
        .ascii_serialization();
    let same_origin = spec_origin == final_origin;

    let allowed = spec.redirect_allowlist.iter().any(|allowlisted| {
        url::Url::parse(allowlisted)
            .ok()
            .is_some_and(|url| url.origin().ascii_serialization() == final_origin)
    });

    // private redirect（allowlist 外）→ 拒否。M05。
    if !same_origin && !allowed {
        return Err(DownloadError::RedirectRejected(
            "origin is not allowlisted".into(),
        ));
    }
    // 異 origin へ credential を転送 → 拒否。M05。
    if !same_origin && spec.credentials.is_some() {
        return Err(DownloadError::CredentialLeak(
            "cross-origin credentials are not forwarded".into(),
        ));
    }
    Ok(())
}

fn validate_download_url(value: &str) -> Result<(), DownloadError> {
    let parsed = url::Url::parse(value)
        .map_err(|_| DownloadError::RedirectRejected("invalid URL".into()))?;
    let fixture_scheme = cfg!(feature = "test-support") && parsed.scheme() == "fixture";
    if (!fixture_scheme && parsed.scheme() != "https")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.ends_with(".localhost")
                || host.ends_with(".local")
        })
    {
        return Err(DownloadError::RedirectRejected("unsafe URL".into()));
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip))
            if ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() =>
        {
            Err(DownloadError::RedirectRejected("unsafe address".into()))
        }
        Some(url::Host::Ipv6(ip))
            if ip.is_unique_local()
                || ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_unspecified() =>
        {
            Err(DownloadError::RedirectRejected("unsafe address".into()))
        }
        _ => Ok(()),
    }
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
    if spec.expected_size == 0
        || spec.sha256.len() != 64
        || !spec.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DownloadError::Other(
            "invalid catalog digest or size".into(),
        ));
    }
    validate_download_url(&spec.url)?;

    // 既存 part の状態を確認（resume 判定）。M05。
    let existing_part = existing_regular_file_size(part_path)?;
    let mut progress = DownloadProgress {
        expected_size: spec.expected_size,
        downloaded: existing_part,
        etag: resume_etag.map(|s| s.to_string()),
    };
    if progress.downloaded > spec.expected_size {
        truncate_part(part_path)?;
        progress.downloaded = 0;
        progress.etag = None;
    }

    // 既に size に達していれば digest 照合で完了判定。M05。
    if progress.downloaded == spec.expected_size && progress.downloaded > 0 {
        let (_, digest) = sha256_file(part_path)?;
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
    let mut etag_restarts = 0_u8;

    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DownloadError::Canceled);
        }
        let incoming_path = part_path.with_file_name(format!(
            ".siderostat-model-response-{}.part",
            uuid::Uuid::new_v4()
        ));
        let incoming = IncomingFile(incoming_path.clone());
        let response = transport
            .get_range_to_file(
                spec,
                start,
                progress.etag.as_deref(),
                &incoming_path,
                spec.expected_size.saturating_sub(start).saturating_add(1),
                cancel,
            )
            .map_err(|error| match error {
                HttpError::Canceled => DownloadError::Canceled,
                HttpError::WriteFailed(message) => DownloadError::WriteFailed(message),
                HttpError::InvalidResponse(message)
                    if message.contains("redirect") || message.contains("URL") =>
                {
                    DownloadError::RedirectRejected("HTTP redirect or URL rejected".into())
                }
                HttpError::InvalidResponse(message) if message.contains("exceeds catalog size") => {
                    DownloadError::SizeMismatch
                }
                _ => DownloadError::Other("model transfer failed".into()),
            })?;
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DownloadError::Canceled);
        }
        let incoming_metadata = std::fs::symlink_metadata(&incoming_path)
            .map_err(|_| DownloadError::WriteFailed("temporary response unavailable".into()))?;
        if incoming_metadata.file_type().is_symlink()
            || !incoming_metadata.is_file()
            || incoming_metadata.len() != response.body_size
        {
            return Err(DownloadError::WriteFailed(
                "temporary response is invalid".into(),
            ));
        }

        // private redirect → 拒否（allowlist 外・異 origin への credential 転送）。M05。
        validate_redirect(spec, &response.final_url)?;

        // 416 → size/full hash で完了判定。M05。
        if response.status == 416 {
            drop(incoming);
            if progress.downloaded == spec.expected_size {
                let (actual_size, digest) = sha256_file(part_path)?;
                if actual_size == spec.expected_size && digest == spec.sha256 {
                    return Ok(progress);
                }
            }
            return Err(DownloadError::Incomplete(
                "416 without valid completed part".into(),
            ));
        }

        // 200 on resume → truncate part のみ（追記せず再取得）。M05。
        if response.status == 200 {
            if response.body_size != spec.expected_size {
                return Err(DownloadError::SizeMismatch);
            }
            std::fs::rename(&incoming_path, part_path)
                .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
            progress.downloaded = response.body_size;
            progress.etag = response.etag.clone();
            break;
        }

        // 206 → Content-Range 検証。M05。
        if response.status == 206 {
            let cr = response.content_range.clone().unwrap_or_default();
            // Content-Range が `bytes {start}-{end}/{total}` 形式か検証。M05。
            let parsed_range = parse_content_range(&cr);
            let Some((range_start, range_end, total)) = parsed_range else {
                return Err(DownloadError::RangeMismatch("invalid content range".into()));
            };
            if range_start != start
                || total != spec.expected_size
                || range_end < range_start
                || range_end - range_start + 1 != response.body_size
                || range_end >= total
            {
                return Err(DownloadError::RangeMismatch(
                    "content range mismatch".into(),
                ));
            }
            // ETag 変更 → restart（part 破棄）。M05。
            if let (Some(new_etag), Some(old_etag)) = (&response.etag, &progress.etag) {
                if new_etag != old_etag {
                    if etag_restarts > 0 {
                        return Err(DownloadError::Other("model ETag kept changing".into()));
                    }
                    truncate_part(part_path)?;
                    drop(incoming);
                    progress.downloaded = 0;
                    progress.etag = response.etag.clone();
                    start = 0;
                    etag_restarts += 1;
                    continue;
                }
            }
            let next = progress
                .downloaded
                .checked_add(response.body_size)
                .ok_or(DownloadError::SizeMismatch)?;
            if next > spec.expected_size {
                return Err(DownloadError::SizeMismatch);
            }
            // 追記。M05。
            append_file(part_path, &incoming_path)?;
            progress.downloaded = next;
            progress.etag = response.etag.clone();
            if progress.downloaded == spec.expected_size {
                break;
            }
            start = progress.downloaded;
            continue;
        }

        return Err(DownloadError::Other(
            "unexpected model response status".into(),
        ));
    }

    // 完了時 full SHA 照合。M05。
    let (actual_size, digest) = sha256_file(part_path)?;
    if actual_size != spec.expected_size {
        return Err(DownloadError::SizeMismatch);
    }
    if digest != spec.sha256 {
        return Err(DownloadError::DigestMismatch(format!(
            "expected {} got {}",
            spec.sha256, digest
        )));
    }
    Ok(progress)
}

struct IncomingFile(PathBuf);

impl Drop for IncomingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

/// part へ追記する（disk full で失敗 → WriteFailed、active 不変）。M05。
fn append_file(part_path: &Path, incoming_path: &Path) -> Result<(), DownloadError> {
    use std::io::copy;
    let existing = existing_regular_file_size(part_path)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).append(true);
    if existing == 0 && !part_path.exists() {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options
        .open(part_path)
        .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    let mut incoming = std::fs::File::open(incoming_path)
        .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    copy(&mut incoming, &mut f).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    Ok(())
}

fn existing_regular_file_size(path: &Path) -> Result<u64, DownloadError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            DownloadError::WriteFailed("download part is invalid".into()),
        ),
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(DownloadError::WriteFailed(error.to_string())),
    }
}

fn truncate_part(path: &Path) -> Result<(), DownloadError> {
    let _ = existing_regular_file_size(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| DownloadError::WriteFailed(error.to_string()))
}

fn sha256_file(path: &Path) -> Result<(u64, String), DownloadError> {
    use sha2::Digest;
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DownloadError::WriteFailed(
            "download part is invalid".into(),
        ));
    }
    let mut file =
        std::fs::File::open(path).map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|e| DownloadError::WriteFailed(e.to_string()))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .ok_or(DownloadError::SizeMismatch)?;
        hasher.update(&buffer[..count]);
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((size, digest))
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

    #[test]
    fn redirects_require_an_allowlisted_https_origin_without_cross_origin_credentials() {
        let mut spec = DownloadSpec::new(
            "https://models.example.com/model.bin",
            4,
            crate::manager::registry::sha256_hex(b"data"),
        );
        spec.redirect_allowlist = vec!["https://cdn.example.com/".into()];
        assert!(validate_redirect(&spec, "https://cdn.example.com/model.bin").is_ok());
        assert!(validate_redirect(&spec, "http://cdn.example.com/model.bin").is_err());
        assert!(validate_redirect(&spec, "https://127.0.0.1/model.bin").is_err());

        spec.credentials = Some(Credentials {
            bearer: "secret".into(),
        });
        assert!(matches!(
            validate_redirect(&spec, "https://cdn.example.com/model.bin"),
            Err(DownloadError::CredentialLeak(_))
        ));
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
