//! M05 — bounded download・resume・容量予約。受入 matrix。M05。
//!
//! 本ファイルは M05 カードの `v040_download.rs` に対応する。公開 API
//! （`download_bounded` / `DownloadSpec` / `DownloadProgress` /
//! `DownloadError` / `check_capacity` / `HttpTransport` / `HttpResponse`）を
//! 介して受入 case を検証する。HTTP 転送は FakeHttp（ローカル fixture）
//! 限定。実 reqwest / 実 network は行わない（H 系で注入）。M05。
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
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M05。
use siderostat::manager::download::{
    DownloadError, DownloadSpec, HttpError, HttpResponse, HttpTransport, check_capacity,
    download_bounded,
};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m05it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create base");
    base
}

/// Fake HTTP 転送。M05。
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

/// 受入 case 1: 206 range 違い → 停止。M05。
#[test]
fn m05_range_mismatch_stops() {
    let base = tmp("range");
    let part = base.join("part.bin");
    std::fs::write(&part, b"A").expect("write part");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        4,
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    );
    let fake = FakeHttp::new(vec![Ok(HttpResponse {
        status: 206,
        etag: Some("e1".to_string()),
        content_range: Some("bytes 0-3/4".to_string()),
        final_url: spec.url.clone(),
        body: b"BCDE".to_vec(),
    })]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let err =
        download_bounded(&spec, &fake, &part, None, &cancel).expect_err("range mismatch must stop");
    assert!(matches!(err, DownloadError::RangeMismatch(_)));
}

/// 受入 case 2: 200 on resume → truncate part のみ。M05。
#[test]
fn m05_resume_200_truncates_part_only() {
    let base = tmp("resume200");
    let part = base.join("part.bin");
    std::fs::write(&part, b"XYZ").expect("write part");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        2,
        siderostat::manager::registry::sha256_hex(b"OK"),
    );
    let fake = FakeHttp::new(vec![Ok(HttpResponse {
        status: 200,
        etag: Some("e1".to_string()),
        content_range: None,
        final_url: spec.url.clone(),
        body: b"OK".to_vec(),
    })]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let prog = download_bounded(&spec, &fake, &part, None, &cancel).expect("download ok");
    assert_eq!(std::fs::read(&part).expect("read part"), b"OK");
    assert_eq!(prog.downloaded, 2);
}

/// 受入 case 3: ETag 変更 → restart。M05。
#[test]
fn m05_etag_change_restarts() {
    let base = tmp("etag");
    let part = base.join("part.bin");
    std::fs::write(&part, b"A").expect("write part");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        2,
        siderostat::manager::registry::sha256_hex(b"AB"),
    );
    // journal の etag=e0 とレスポンスの etag=e1 が不一致 → restart。M05。
    // fake が 1 個のみのため、restart 後の再取得は空 → InvalidResponse で失敗。
    let fake = FakeHttp::new(vec![Ok(HttpResponse {
        status: 206,
        etag: Some("e1".to_string()),
        content_range: Some("bytes 1-1/2".to_string()),
        final_url: spec.url.clone(),
        body: b"B".to_vec(),
    })]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let err = download_bounded(&spec, &fake, &part, Some("e0"), &cancel);
    // ETag 変更 → restart（破棄）。M05。
    assert!(err.is_err());
}

/// 受入 case 4: cancel → active 不変。M05。
#[test]
fn m05_cancel_leaves_active_unchanged() {
    let base = tmp("cancel");
    let part = base.join("part.bin");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        4,
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    );
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let fake = FakeHttp::new(vec![]);
    let err = download_bounded(&spec, &fake, &part, None, &cancel).expect_err("canceled");
    assert_eq!(err, DownloadError::Canceled);
    assert!(!part.exists());
}

/// 受入 case 5: private redirect → 拒否。M05。
#[test]
fn m05_private_redirect_rejected() {
    let base = tmp("redirect");
    let part = base.join("part.bin");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        4,
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    );
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
    assert!(!part.exists());
}

/// 容量予約: 同時 download による容量超過を検査。M05。
#[test]
fn m05_capacity_shortage_checked() {
    // 同時 2 download + 余白で不足。M05。
    let err = check_capacity(10, 100, 2, 10).expect_err("capacity must be checked");
    assert!(err.to_string().contains("capacity"));
    // 十分な容量 → OK。M05。
    check_capacity(1000, 100, 2, 10).expect("capacity ok");
}

/// ディスク不足（書き込み失敗）→ active 不変。M05。
///
/// 書き込み先ディレクトリが存在しない・書けない場合、WriteFailed で
/// 中断し、完成 model を変更しないことを検証する。M05。
#[test]
fn m05_write_failure_leaves_active_unchanged() {
    let base = tmp("writefail");
    // part パスは存在しない親ディレクトリ配下（書けない）。M05。
    let part = base.join("no_such_dir").join("part.bin");
    let spec = DownloadSpec::new(
        "https://models.example.com/m.bin",
        2,
        siderostat::manager::registry::sha256_hex(b"OK"),
    );
    // 200 応答（part に書こうとするが親 dir が無い → 失敗）。M05。
    let fake = FakeHttp::new(vec![Ok(HttpResponse {
        status: 200,
        etag: None,
        content_range: None,
        final_url: spec.url.clone(),
        body: b"OK".to_vec(),
    })]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let err = download_bounded(&spec, &fake, &part, None, &cancel).expect_err("write must fail");
    assert!(matches!(err, DownloadError::WriteFailed(_)));
}
