//! Codex Web Search Bridge — SearchBackend 境界と SearchResult 正規化。W04。
//!
//! C05 に基づき、検索 backend の抽象境界（[`SearchBackend`]）と、検索結果
//! の正規化（dedupe・件数制限・URL 除外）を定義する。SearXNG 実装は
//! [`crate::websearch::searxng`]。
//!
//! 契約: CONTRACTS.md C05 / SearchBackend・SearchResult。W04。
//!
//! 受入 case（全て必須）:
//! - 入力: 403/429/timeout/HTML/壊れJSON/空results → 別error
//! - 入力: 重複URL → dedupe
//! - 入力: 11件 → 5件
//! - 入力: javascript/file URL → 除外

use url::Url;

/// 検索結果。C05 の固定形。W04。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: Url,
    pub snippet: String,
}

/// 検索 backend の将来型。C05 の固定形。W04。
pub type SearchFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<SearchResult>, SearchError>> + Send + 'a>,
>;

/// 検索 backend の抽象。C05 の固定形。W04。
pub trait SearchBackend: Send + Sync {
    /// 検索を実行し、正規化済みの結果を返す。上限（既定5/最大10）は
    /// 実装側で適用する。
    fn search<'a>(&'a self, query: &'a str) -> SearchFuture<'a>;
}

/// 検索エラー分類。C05 の固定形。W04。
///
/// - Forbidden: 403。provider の全 403 を CAPTCHA と断定しない。
/// - RateLimited: 429。
/// - Captcha: HTML challenge が観測できたときだけ分類（200 + HTML body）。
/// - Timeout: 通信 timeout。
/// - InvalidJson: JSON が壊れている。
/// - NoResults: 空 results。
/// - ResponseTooLarge: provider body 上限超過。
/// - Unavailable: 5xx / 接続不可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    Forbidden,
    RateLimited,
    Captcha,
    Timeout,
    InvalidJson,
    NoResults,
    ResponseTooLarge,
    Unavailable,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            SearchError::Forbidden => "forbidden",
            SearchError::RateLimited => "rate_limited",
            SearchError::Captcha => "captcha",
            SearchError::Timeout => "timeout",
            SearchError::InvalidJson => "invalid_json",
            SearchError::NoResults => "no_results",
            SearchError::ResponseTooLarge => "response_too_large",
            SearchError::Unavailable => "unavailable",
        };
        write!(f, "{name}")
    }
}

impl std::error::Error for SearchError {}

/// 結果の既定上限（C05: result 既定5）。W04。
pub const DEFAULT_MAX_RESULTS: usize = 5;
/// 結果の最大上限（C05: result 最大10）。W04。
pub const MAX_RESULTS_LIMIT: usize = 10;
/// snippet の上限（C05: snippet 4096 文字）。W04。
pub const MAX_SNIPPET_CHARS: usize = 4096;

/// 結果 URL が除外すべき scheme か判定する。W04。
///
/// C05: 結果 URL の fetch は実装しない。http/https 以外（javascript:,
/// file: 等）は引用集合から除く。
fn excluded_scheme(url: &Url) -> bool {
    !matches!(url.scheme(), "http" | "https")
}

/// 検索結果を正規化する。W04。
///
/// - javascript:/file: 等の非 http/https URL を除外する。
/// - 重複 URL を dedupe する（順序保持）。
/// - 既定 5 件（最大10）に制限する。
/// - snippet を上限（4096 文字）に切詰める。
pub fn normalize_results(mut results: Vec<SearchResult>, max_results: usize) -> Vec<SearchResult> {
    let max = max_results.clamp(1, MAX_RESULTS_LIMIT);
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<SearchResult> = Vec::with_capacity(max);
    for r in results.drain(..) {
        if out.len() >= max {
            break;
        }
        // 非 http/https URL（javascript:/file: 等）を除外。
        if excluded_scheme(&r.url) {
            continue;
        }
        // 重複 URL を dedupe（順序保持）。
        if !seen.insert(r.url.as_str().to_string()) {
            continue;
        }
        // snippet を上限に切詰める。C05: snippet 4096 文字。
        let snippet: String = r.snippet.chars().take(MAX_SNIPPET_CHARS).collect();
        out.push(SearchResult {
            title: r.title,
            url: r.url,
            snippet,
        });
    }
    out
}
