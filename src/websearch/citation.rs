//! Codex Web Search Bridge — citation 参照 ID と Unicode 範囲の生成。W06。
//!
//! C05 に基づき、検索結果に request 内だけの source ID を付け、モデルの引用
//! marker を実 URL の可視引用 text に変換してから範囲を算出する。偽 ID は
//! 拒否。重複引用は各範囲一致。marker 分割（未確定）は送信しない。final
//! text と annotation を一体生成する。
//!
//! A03 wire-profile の key_finding: Codex 0.153.4 は url_citation annotation を
//! 持たないため、引用は model 回答 text 内の文字として扱う。W06 は引用
//! marker を可視引用 text に変換し、byte 境界を固定する。W06。
//!
//! 契約: CONTRACTS.md C05 / CitedAnswer・UrlCitation。W06。
//!
//! 受入 case（全て必須）:
//! - 入力: 日本語/絵文字/結合文字 → 同じ引用範囲
//! - 入力: 偽ID → 拒否
//! - 入力: 重複引用 → 各範囲一致
//! - 入力: marker分割 → 未確定部分を送信しない

use super::backend::SearchResult;

/// 1 件の URL citation。W06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlCitation {
    /// 実結果 URL（引用集合に含まれるもの）。W06。
    pub url: String,
    /// 可視引用 text の byte 開始 index（final text 内）。W06。
    pub start: usize,
    /// 可視引用 text の byte 終了 index（排他）。W06。
    pub end: usize,
    /// request 内だけの source ID（引用集合の逆引き検査用）。W06。
    pub source_id: String,
}

/// 引用付き最終回答。W06。
///
/// `text`（可視引用 text を埋め込んだ最終 text）と `citations`（各引用の
/// byte 範囲）を一体生成する。final text と annotation が一致する。W06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitedAnswer {
    /// 最終 text（可視引用 text を埋め込み済み）。W06。
    pub text: String,
    /// 各引用の byte 範囲（順序保持）。W06。
    pub citations: Vec<UrlCitation>,
}

/// 引用 marker を表す。W06。
///
/// `[N]` / `[1][2]` 等。marker は byte 位置で走査する。W06。
struct CitationMarker {
    /// marker 全体の byte 開始。W06。
    start: usize,
    /// marker 全体の byte 終了（排他）。W06。
    end: usize,
    /// 引用番号 N。W06。
    id: usize,
}

/// text 中の引用 marker を走査する。W06。
///
/// `[N]` を byte 位置で検出する。`[1` のように閉じ括弧が無い未確定部分が
/// 末尾に残る場合は `incomplete` を true にする（送信しない）。W06。
fn scan_markers(text: &str) -> (Vec<CitationMarker>, bool) {
    let bytes = text.as_bytes();
    let mut markers = Vec::new();
    let mut i = 0;
    let mut incomplete = false;

    while i < bytes.len() {
        if bytes[i] == b'[' {
            // 開き括弧。数字列を読む。W06。
            let mut j = i + 1;
            let mut digits = String::new();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                digits.push(bytes[j] as char);
                j += 1;
            }
            if digits.is_empty() {
                // `[` の直後が数字でない → marker ではない。次の文字へ。W06。
                i += 1;
                continue;
            }
            // 閉じ括弧を確認。W06。
            if j < bytes.len() && bytes[j] == b']' {
                let id: usize = digits.parse().unwrap_or(0);
                markers.push(CitationMarker {
                    start: i,
                    end: j + 1,
                    id,
                });
                i = j + 1;
            } else {
                // 閉じ括弧が無い（未確定 marker）。W06。
                // この先に閉じ括弧があるか確認し、無ければ incomplete。W06。
                let rest = &bytes[j..];
                if !rest.contains(&b']') {
                    incomplete = true;
                }
                // 閉じ括弧があっても数字だけが中途なら marker としない。W06。
                i = j.max(i + 1);
            }
        } else {
            i += 1;
        }
    }

    (markers, incomplete)
}

/// 検索結果から source ID を付けた引用集合を構築する。W06。
///
/// source ID は request 内だけの一意 ID。URL を逆引き検査に使用する。W06。
pub fn build_sources(results: &[SearchResult]) -> Vec<UrlCitation> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| UrlCitation {
            url: r.url.to_string(),
            start: 0,
            end: 0,
            source_id: format!("src_{}", i + 1),
        })
        .collect()
}

/// 引用集合の URL から source ID を逆引きする。W06。
fn reverse_lookup<'a>(sources: &'a [UrlCitation], url: &str) -> Option<&'a str> {
    sources
        .iter()
        .find(|s| s.url == url)
        .map(|s| s.source_id.as_str())
}

/// 引用 marker を実 URL の可視引用 text に変換し、byte 範囲を算出する。W06。
///
/// - model の回答 text 内の `[N]` を、N 番目の検索結果の可視引用 text
///   （`[N] (URL)`）に置換する。
/// - 偽 ID（結果集合に無い N）は拒否（CitationInvalid 相当）。W06。
/// - 重複引用は各出現を独立に範囲計算（各範囲一致）。W06。
/// - marker 分割（`[1` 等の未確定部分）は未確定として送信しない。W06。
///
/// 範囲は byte index。日本語・絵文字・結合文字をまたぐ引用でも、可視引用
/// text 全体が一つの byte 範囲として一致する（受入 case）。W06。
pub fn render_citations(
    model_text: &str,
    sources: &[UrlCitation],
) -> Result<CitedAnswer, CitationError> {
    let mut text = String::new();
    let mut citations: Vec<UrlCitation> = Vec::new();
    let mut last = 0;

    // marker 分割（未確定）の検出。未確定部分を送信しないため、
    // 全体を IncompleteMarker として扱う。W06。
    let (markers, incomplete) = scan_markers(model_text);
    if incomplete {
        return Err(CitationError::IncompleteMarker);
    }

    for m in markers {
        // marker 前の text をコピー。W06。
        text.push_str(&model_text[last..m.start]);
        let n = m.id;

        // 引用集合から URL を逆引き。W06。
        let url = sources
            .get(n.checked_sub(1).ok_or(CitationError::InvalidId)?)
            .map(|s| s.url.clone())
            .ok_or(CitationError::InvalidId)?;
        let source_id = reverse_lookup(sources, &url)
            .ok_or(CitationError::InvalidId)?
            .to_string();

        // 可視引用 text。W06。
        let visible = format!("[{n}] ({url})");
        let start = text.len();
        text.push_str(&visible);
        let end = text.len();

        citations.push(UrlCitation {
            url,
            start,
            end,
            source_id,
        });

        last = m.end;
    }

    // 残り text。W06。
    text.push_str(&model_text[last..]);

    Ok(CitedAnswer { text, citations })
}

/// citation エラー。W06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CitationError {
    /// 偽 ID（結果集合に無い）。W06。
    InvalidId,
    /// marker 分割（未確定）で送信不可。W06。
    IncompleteMarker,
    /// 引用 URL が結果集合に無い（逆引き失敗）。W06。
    UnknownUrl,
}

impl std::fmt::Display for CitationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            CitationError::InvalidId => "invalid_citation_id",
            CitationError::IncompleteMarker => "incomplete_citation_marker",
            CitationError::UnknownUrl => "unknown_citation_url",
        };
        write!(f, "{name}")
    }
}

impl std::error::Error for CitationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Vec<UrlCitation> {
        let results = vec![
            SearchResult {
                title: "A".into(),
                url: url::Url::parse("http://example.com/a").unwrap(),
                snippet: "sa".into(),
            },
            SearchResult {
                title: "B".into(),
                url: url::Url::parse("http://example.com/b").unwrap(),
                snippet: "sb".into(),
            },
        ];
        build_sources(&results)
    }

    #[test]
    fn renders_citation_with_byte_range() {
        let s = sources();
        let ans = render_citations("see [1] for details", &s).unwrap();
        assert_eq!(ans.text, "see [1] (http://example.com/a) for details");
        assert_eq!(ans.citations.len(), 1);
        assert_eq!(ans.citations[0].start, 4);
        // 可視引用 text 全体が範囲。残り " for details"（12 文字）を除く。W06。
        assert_eq!(ans.citations[0].end, ans.text.len() - 12);
        assert_eq!(ans.citations[0].source_id, "src_1");
    }

    #[test]
    fn japanese_emoji_combining_keeps_same_range() {
        let s = sources();
        // 日本語・絵文字・結合文字（e + U+0301）をまたぐ引用。W06。
        let model = "日本語🙂e\u{301} [1] 参照";
        let ans = render_citations(model, &s).unwrap();
        // 可視引用 text 全体が一つの byte 範囲として一致。W06。
        let citation = &ans.citations[0];
        let visible = &ans.text[citation.start..citation.end];
        assert_eq!(visible, "[1] (http://example.com/a)");
        assert!(ans.text.contains("日本語🙂e\u{301}"));
    }

    #[test]
    fn fake_id_is_rejected() {
        let s = sources();
        let err = render_citations("see [99]", &s).unwrap_err();
        assert_eq!(err, CitationError::InvalidId);
    }

    #[test]
    fn id_zero_is_rejected() {
        let s = sources();
        let err = render_citations("see [0]", &s).unwrap_err();
        assert_eq!(err, CitationError::InvalidId);
    }

    #[test]
    fn duplicate_citations_each_range_matches() {
        let s = sources();
        let ans = render_citations("[1] and [1] again", &s).unwrap();
        assert_eq!(ans.citations.len(), 2);
        // 各引用の可視 text がそれぞれ一致。W06。
        for c in &ans.citations {
            let visible = &ans.text[c.start..c.end];
            assert_eq!(visible, "[1] (http://example.com/a)");
        }
    }

    #[test]
    fn multiple_different_citations() {
        let s = sources();
        let ans = render_citations("a [1] b [2] c", &s).unwrap();
        assert_eq!(ans.citations.len(), 2);
        assert_eq!(
            &ans.text[ans.citations[0].start..ans.citations[0].end],
            "[1] (http://example.com/a)"
        );
        assert_eq!(
            &ans.text[ans.citations[1].start..ans.citations[1].end],
            "[2] (http://example.com/b)"
        );
    }

    #[test]
    fn incomplete_marker_is_not_sent() {
        let s = sources();
        // 未確定 marker（閉じ括弧が無い）。W06。
        let err = render_citations("see [1", &s).unwrap_err();
        assert_eq!(err, CitationError::IncompleteMarker);
    }

    #[test]
    fn no_citations_returns_original_text() {
        let s = sources();
        let ans = render_citations("plain text", &s).unwrap();
        assert_eq!(ans.text, "plain text");
        assert!(ans.citations.is_empty());
    }
}
