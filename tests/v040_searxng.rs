//! W04 — SearXNG adapter・正規化・エラー分類。受入 matrix。W04。
//!
//! 本ファイルは W04 カードの target_commands にある `v040_searxng.rs` に
//! 対応する。公開 API（`siderostat::websearch::searxng::classify_response` /
//! `SearxngBackend` / `SearxngTransport` / `SearxngHttpResponse`、
//! `siderostat::websearch::backend::normalize_results` / `SearchResult` /
//! `SearchError`）を介して受入 case を検証する。SearXNG への実 HTTP 送信は
//! 行わず、fake transport 境界で応答を注入する（dry-run / fake 境界）。W04。
//!
//! 受入 case（全て必須）:
//! - 入力: 403/429/timeout/HTML/壊れJSON/空results → 別error
//! - 入力: 重複URL → dedupe
//! - 入力: 11件 → 5件
//! - 入力: javascript/file URL → 除外
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。W04。

use siderostat::websearch::backend::{SearchError, SearchResult, normalize_results};
use siderostat::websearch::searxng::{
    SearxngBackend, SearxngHttpResponse, SearxngTransport, classify_response,
};
use std::sync::Mutex;
use url::Url;

/// fake transport。設定した応答を返す（実 HTTP なし）。W04。
struct FakeTransport {
    response: Mutex<Option<SearxngHttpResponse>>,
}

impl FakeTransport {
    fn new(resp: SearxngHttpResponse) -> Self {
        Self {
            response: Mutex::new(Some(resp)),
        }
    }
}

impl SearxngTransport for FakeTransport {
    fn post_search(
        &self,
        _endpoint: &Url,
        _query: &str,
        _api_key: Option<&str>,
    ) -> Result<SearxngHttpResponse, SearchError> {
        let mut g = self.response.lock().unwrap();
        Ok(g.take().expect("fake response"))
    }
}

fn resp(status: u16, ct: &str, body: &str) -> SearxngHttpResponse {
    SearxngHttpResponse {
        status,
        body: body.as_bytes().to_vec(),
        content_type: Some(ct.into()),
    }
}

fn result(title: &str, url: &str, snippet: &str) -> SearchResult {
    SearchResult {
        title: title.into(),
        url: Url::parse(url).expect("url"),
        snippet: snippet.into(),
    }
}

/// 受入 case 1a: 403 → Forbidden（provider の全 403 を CAPTCHA と断定しない）。W04。
#[test]
fn w04_403_is_forbidden_not_captcha() {
    let r = classify_response(resp(403, "text/plain", "denied"), 5);
    assert_eq!(r, Err(SearchError::Forbidden));
}

/// 受入 case 1b: 429 → RateLimited。W04。
#[test]
fn w04_429_is_rate_limited() {
    let r = classify_response(resp(429, "text/plain", "slow down"), 5);
    assert_eq!(r, Err(SearchError::RateLimited));
}

/// 受入 case 1c: 500 → Unavailable。W04。
#[test]
fn w04_500_is_unavailable() {
    let r = classify_response(resp(500, "text/plain", "boom"), 5);
    assert_eq!(r, Err(SearchError::Unavailable));
}

/// 受入 case 1d: 200 + HTML body → Captcha（HTML challenge 観測時のみ）。W04。
#[test]
fn w04_html_body_is_captcha() {
    let r = classify_response(resp(200, "text/html", "<html>challenge</html>"), 5);
    assert_eq!(r, Err(SearchError::Captcha));
}

/// 受入 case 1e: 200 + 壊れ JSON → InvalidJson。W04。
#[test]
fn w04_broken_json_is_invalid_json() {
    let r = classify_response(resp(200, "application/json", "{not json"), 5);
    assert_eq!(r, Err(SearchError::InvalidJson));
}

/// 受入 case 1f: 200 + 空 results → NoResults。W04。
#[test]
fn w04_empty_results_is_no_results() {
    let r = classify_response(resp(200, "application/json", r#"{"results":[]}"#), 5);
    assert_eq!(r, Err(SearchError::NoResults));
}

/// 受入 case 1g: results キー無し → NoResults。W04。
#[test]
fn w04_missing_results_is_no_results() {
    let r = classify_response(resp(200, "application/json", r#"{"other":1}"#), 5);
    assert_eq!(r, Err(SearchError::NoResults));
}

/// 受入 case 1h: provider body 上限超過 → ResponseTooLarge。W04。
#[test]
fn w04_oversize_provider_body_is_response_too_large() {
    let big = "x".repeat(siderostat::websearch::searxng::MAX_PROVIDER_BODY_BYTES + 1);
    let r = classify_response(resp(200, "application/json", &big), 5);
    assert_eq!(r, Err(SearchError::ResponseTooLarge));
}

/// 受入 case 2: 重複URL → dedupe（順序保持）。W04。
#[test]
fn w04_duplicate_urls_are_deduped() {
    let results = vec![
        result("a", "http://example.com/1", "s1"),
        result("b", "http://example.com/1", "dup"), // 重複 URL。
        result("c", "http://example.com/2", "s2"),
    ];
    let out = normalize_results(results, 5);
    assert_eq!(out.len(), 2, "duplicate urls must be deduped");
    assert_eq!(out[0].title, "a");
    assert_eq!(out[1].title, "c");
}

/// 受入 case 3: 11件 → 5件（既定上限）。W04。
#[test]
fn w04_eleven_results_limited_to_five() {
    let results: Vec<SearchResult> = (0..11)
        .map(|i| result(&format!("t{i}"), &format!("http://example.com/{i}"), "s"))
        .collect();
    let out = normalize_results(results, 5);
    assert_eq!(out.len(), 5, "11 results must be limited to default 5");
    assert_eq!(out[0].title, "t0");
    assert_eq!(out[4].title, "t4");
}

/// 受入 case 3b: 上限10を超える要求は10に clamp。W04。
#[test]
fn w04_max_results_clamped_to_ten() {
    let results: Vec<SearchResult> = (0..15)
        .map(|i| result(&format!("t{i}"), &format!("http://example.com/{i}"), "s"))
        .collect();
    let out = normalize_results(results, 100);
    assert_eq!(out.len(), 10, "max_results must clamp to 10");
}

/// 受入 case 4: javascript/file URL → 除外。W04。
#[test]
fn w04_javascript_and_file_urls_excluded() {
    let results = vec![
        result("js", "javascript:alert(1)", "x"),
        result("file", "file:///etc/passwd", "y"),
        result("ok", "http://example.com/ok", "z"),
    ];
    let out = normalize_results(results, 5);
    assert_eq!(out.len(), 1, "javascript/file urls must be excluded");
    assert_eq!(out[0].title, "ok");
    assert_eq!(out[0].url.as_str(), "http://example.com/ok");
}

/// 受入 case 4b: ftp URL も除外。W04。
#[test]
fn w04_ftp_url_excluded() {
    let results = vec![
        result("ftp", "ftp://example.com/f", "x"),
        result("ok", "https://example.com/ok", "y"),
    ];
    let out = normalize_results(results, 5);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].title, "ok");
}

/// SearXNG の正常応答から正規化された結果を返す。W04。
#[test]
fn w04_valid_json_normalizes_results() {
    let body = r#"{
      "results": [
        {"title": "A", "url": "http://example.com/a", "content": "snippet a"},
        {"title": "B", "url": "http://example.com/b", "content": "snippet b"}
      ]
    }"#;
    let r = classify_response(resp(200, "application/json", body), 5);
    let out = r.expect("valid json");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].title, "A");
    assert_eq!(out[0].snippet, "snippet a");
    assert_eq!(out[1].title, "B");
}

/// SearxngBackend を fake transport で駆動する。W04。
#[test]
fn w04_backend_search_with_fake_transport() {
    let endpoint = Url::parse("http://searxng.local").expect("url");
    let body = r#"{"results":[{"title":"X","url":"http://example.com/x","content":"s"}]}"#;
    let transport = Box::new(FakeTransport::new(resp(200, "application/json", body)));
    let backend = SearxngBackend::new(endpoint, None, transport);
    let out = backend.search_blocking("query").expect("search");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].title, "X");
}

/// 空 query → NoResults。W04。
#[test]
fn w04_empty_query_is_no_results() {
    let endpoint = Url::parse("http://searxng.local").expect("url");
    let transport = Box::new(FakeTransport::new(resp(200, "application/json", "{}")));
    let backend = SearxngBackend::new(endpoint, None, transport);
    let err = backend.search_blocking("   ").expect_err("empty query");
    assert_eq!(err, SearchError::NoResults);
}
