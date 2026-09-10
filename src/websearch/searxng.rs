//! Codex Web Search Bridge — SearXNG adapter・正規化・エラー分類。W04。
//!
//! C05 に基づき、固定 endpoint `/search` へ form POST（format=json）で送信し、
//! 結果を正規化する。HTTP status と parse/engine error/empty を分離し、
//! HTML challenge は観測できる時だけ Captcha に分類する。provider の全 403
//! を CAPTCHA と断定しない。redirect 無効。検索結果 URL を fetch しない。
//! 認証 header・query 本文がログに漏れない。
//!
//! 契約: CONTRACTS.md C05 / SearchBackend・SearchResult。W04。
//!
//! 受入 case（全て必須）:
//! - 入力: 403/429/timeout/HTML/壊れJSON/空results → 別error
//! - 入力: 重複URL → dedupe
//! - 入力: 11件 → 5件
//! - 入力: javascript/file URL → 除外

use super::backend::{SearchError, SearchResult};
use url::Url;

/// SearXNG JSON endpoint の固定 path。W04。
const SEARXNG_SEARCH_PATH: &str = "/search";

/// SearXNG 応答の body 上限（C05: provider body1MiB）。W04。
pub const MAX_PROVIDER_BODY_BYTES: usize = 1024 * 1024;

/// SearXNG の HTTP 応答（テスト用 fake 境界で注入する）。W04。
#[derive(Debug, Clone)]
pub struct SearxngHttpResponse {
    pub status: u16,
    /// 応答 body。認証・query 本文を含まない（SearXNG 応答のみ）。W04。
    pub body: Vec<u8>,
    /// Content-Type。HTML challenge 判定に使用。W04。
    pub content_type: Option<String>,
}

/// SearXNG への HTTP 送信を行う抽象。W04。
///
/// 本番では reqwest を使うが、テストは fake 境界で応答を注入する。
/// この trait を通すことで、実プロセス・実ネットワーク無しでエラー分類と
/// 正規化を検証できる。
pub trait SearxngTransport: Send + Sync {
    /// `/search?format=json` へ form POST を送信する。redirect 無効。W04。
    fn post_search(
        &self,
        endpoint: &Url,
        query: &str,
        api_key: Option<&str>,
    ) -> Result<SearxngHttpResponse, SearchError>;
}

/// 本番の reqwest transport。W04。
///
/// 認証 header（api_key）は SearXNG 固有 credential。Bridge client 認証
/// （bearer_token）とは別。query 本文・認証をログに出さない。redirect 無効。W04。
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        // redirect 無効。C05: 検索結果 URL の fetch は実装しない。W04。
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self { client }
    }
}

impl SearxngTransport for ReqwestTransport {
    fn post_search(
        &self,
        endpoint: &Url,
        query: &str,
        api_key: Option<&str>,
    ) -> Result<SearxngHttpResponse, SearchError> {
        // 固定 endpoint `/search`。C05。W04。
        let url = endpoint
            .join(SEARXNG_SEARCH_PATH)
            .map_err(|_| SearchError::Unavailable)?;
        // form POST format=json（application/x-www-form-urlencoded）。C05。W04。
        // query 本文はログに出さない（認証 header と同様）。W04。
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .append_pair("format", "json")
            .finish();
        let mut request = self.client.post(url).header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        );
        if let Some(key) = api_key {
            // SearXNG 固有 api_key。Bridge の bearer とは別。ログに出さない。
            request = request.header("X-Api-Key", key);
        }
        // timeout は W04 では固定せず、上位（W05 の search 上限）で扱う。W04。
        let resp = request
            .body(body)
            .send()
            .map_err(|_| SearchError::Unavailable)?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        // provider body 上限。超過は ResponseTooLarge。W04。
        let body = resp
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|_| SearchError::Unavailable)?;
        if body.len() > MAX_PROVIDER_BODY_BYTES {
            return Err(SearchError::ResponseTooLarge);
        }
        Ok(SearxngHttpResponse {
            status,
            body,
            content_type,
        })
    }
}

/// SearXNG adapter。固定 endpoint /search へ form POST し、結果を正規化する。W04。
pub struct SearxngBackend {
    /// SearXNG base URL。固定 endpoint /search へ接続。W04。
    pub endpoint: Url,
    /// SearXNG 固有 api_key（任意）。W04。
    pub api_key: Option<String>,
    transport: Box<dyn SearxngTransport>,
    /// 結果上限（既定5）。W04。
    max_results: usize,
}

impl SearxngBackend {
    /// 新しい SearXNG backend を構築する。W04。
    pub fn new(
        endpoint: Url,
        api_key: Option<String>,
        transport: Box<dyn SearxngTransport>,
    ) -> Self {
        Self {
            endpoint,
            api_key,
            transport,
            max_results: super::backend::DEFAULT_MAX_RESULTS,
        }
    }

    /// 結果上限を設定する（既定5、上限10）。W04。
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max.clamp(1, super::backend::MAX_RESULTS_LIMIT);
        self
    }

    /// 検索を実行する。W04。
    pub fn search_blocking(&self, query: &str) -> Result<Vec<SearchResult>, SearchError> {
        if query.trim().is_empty() {
            return Err(SearchError::NoResults);
        }
        let resp = self
            .transport
            .post_search(&self.endpoint, query, self.api_key.as_deref())?;
        classify_response(resp, self.max_results)
    }
}

/// SearXNG の応答を分類・正規化する。W04。
///
/// HTTP status と parse/engine error/empty を分離する。provider の全 403 を
/// CAPTCHA と断定しない。HTML challenge は観測できる時だけ Captcha に分類
/// する（200 + HTML body）。
pub fn classify_response(
    resp: SearxngHttpResponse,
    max_results: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    // HTTP status を分類する。W04。
    match resp.status {
        403 => {
            // 403 は Forbidden。provider の全 403 を CAPTCHA と断定しない。
            // JSON 無効 403 は backend 構成エラーとして表示する。
            return Err(SearchError::Forbidden);
        }
        429 => {
            return Err(SearchError::RateLimited);
        }
        500..=599 => {
            return Err(SearchError::Unavailable);
        }
        200 => {}
        other => {
            // その他（4xx 等）は Unavailable。W04。
            let _ = other;
            return Err(SearchError::Unavailable);
        }
    }

    // provider body 上限。超過は ResponseTooLarge。W04。classify_response でも
    // 検査する（transport 実装に依存しない）。W04。
    if resp.body.len() > MAX_PROVIDER_BODY_BYTES {
        return Err(SearchError::ResponseTooLarge);
    }

    // 200 + HTML body → HTML challenge。観測できる時だけ Captcha に分類。W04。
    let is_html = resp
        .content_type
        .as_deref()
        .map(|ct| ct.contains("text/html"))
        .unwrap_or(false);
    if is_html {
        return Err(SearchError::Captcha);
    }

    // JSON を parse する。壊れ JSON → InvalidJson。W04。
    let parsed: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|_| SearchError::InvalidJson)?;

    // results を抽出する。空 results → NoResults。W04。
    let results = parsed.get("results").and_then(|r| r.as_array());
    let results = match results {
        Some(arr) => arr,
        None => return Err(SearchError::NoResults),
    };

    let mut out = Vec::with_capacity(results.len());
    for item in results {
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let url_str = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Ok(url) = Url::parse(url_str) else {
            // 不正 URL は除外（引用集合に入れない）。W04。
            continue;
        };
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }

    // 正規化: dedupe・件数制限・非 http/https URL 除外・snippet 上限。W04。
    let normalized = super::backend::normalize_results(out, max_results);
    // 正規化後に 0 件（全て除外 / 元々空）なら NoResults。W04。
    if normalized.is_empty() {
        return Err(SearchError::NoResults);
    }
    Ok(normalized)
}
