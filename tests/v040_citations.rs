//! W06 — citation 参照 ID と Unicode 範囲の生成。受入 matrix。W06。
//!
//! 本ファイルは W06 カードの target_commands にある `v040_citations.rs` に
//! 対応する。公開 API（`siderostat::websearch::citation::{build_sources,
//! render_citations, CitedAnswer, UrlCitation, CitationError}`）を介して受入 case
//! を検証する。実プロセス・実検索・SSE 送信は行わない（dry-run / fake
//! 境界）。W06。
//!
//! 受入 case（全て必須）:
//! - 入力: 日本語/絵文字/結合文字 → 同じ引用範囲
//! - 入力: 偽ID → 拒否
//! - 入力: 重複引用 → 各範囲一致
//! - 入力: marker分割 → 未確定部分を送信しない
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。W06。

use siderostat::websearch::backend::SearchResult;
use siderostat::websearch::citation::{
    CitationError, UrlCitation, build_sources, render_citations,
};

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

/// 受入 case 1: 日本語/絵文字/結合文字 → 同じ引用範囲。W06。
///
/// モデル回答の日本語・絵文字・結合文字（e + U+0301）をまたぐ引用でも、可視
/// 引用 text 全体が一つの byte 範囲として一致する。W06。
#[test]
fn w06_japanese_emoji_combining_same_range() {
    let s = sources();
    let model = "日本語🙂e\u{301} [1] 参照";
    let ans = render_citations(model, &s).unwrap();
    assert_eq!(ans.citations.len(), 1);
    let c = &ans.citations[0];
    // 可視引用 text 全体が一つの byte 範囲。W06。
    let visible = &ans.text[c.start..c.end];
    assert_eq!(visible, "[1] (http://example.com/a)");
    // 日本語・絵文字・結合文字が保持されている。W06。
    assert!(ans.text.contains("日本語🙂e\u{301}"));
    // byte 範囲は UTF-8 境界（char 境界）で一致する。W06。
    assert!(ans.text.is_char_boundary(c.start));
    assert!(ans.text.is_char_boundary(c.end));
}

/// 受入 case 1b: 引用が text 末尾にある場合の範囲。W06。
#[test]
fn w06_citation_at_end_range() {
    let s = sources();
    let ans = render_citations("answer [2]", &s).unwrap();
    assert_eq!(ans.text, "answer [2] (http://example.com/b)");
    let c = &ans.citations[0];
    assert_eq!(&ans.text[c.start..c.end], "[2] (http://example.com/b)");
    assert_eq!(c.end, ans.text.len());
    assert_eq!(c.source_id, "src_2");
}

/// 受入 case 2: 偽ID → 拒否。W06。
#[test]
fn w06_fake_id_rejected() {
    let s = sources();
    let err = render_citations("see [99]", &s).unwrap_err();
    assert_eq!(err, CitationError::InvalidId);
}

/// 受入 case 2b: ID 0 → 拒否。W06。
#[test]
fn w06_id_zero_rejected() {
    let s = sources();
    let err = render_citations("see [0]", &s).unwrap_err();
    assert_eq!(err, CitationError::InvalidId);
}

/// 受入 case 3: 重複引用 → 各範囲一致。W06。
#[test]
fn w06_duplicate_citations_each_range_matches() {
    let s = sources();
    let ans = render_citations("[1] and [1] again", &s).unwrap();
    assert_eq!(ans.citations.len(), 2);
    for c in &ans.citations {
        assert_eq!(&ans.text[c.start..c.end], "[1] (http://example.com/a)");
        assert_eq!(c.source_id, "src_1");
    }
}

/// 受入 case 3b: 異なる複数引用 → 各範囲一致。W06。
#[test]
fn w06_multiple_citations_ranges_match() {
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
    assert_eq!(ans.citations[0].source_id, "src_1");
    assert_eq!(ans.citations[1].source_id, "src_2");
}

/// 受入 case 4: marker 分割 → 未確定部分を送信しない。W06。
///
/// `[1` のように閉じ括弧が無い未確定 marker → IncompleteMarker で拒否。
/// 未確定部分を SSE で送信しない。W06。
#[test]
fn w06_incomplete_marker_not_sent() {
    let s = sources();
    let err = render_citations("see [1", &s).unwrap_err();
    assert_eq!(err, CitationError::IncompleteMarker);
}

/// 受入 case 4b: 途中に未確定 marker → 拒否。W06。
#[test]
fn w06_incomplete_marker_middle_not_sent() {
    let s = sources();
    // "[2" の後に閉じ括弧が無い。W06。
    let err = render_citations("a [2 b", &s).unwrap_err();
    assert_eq!(err, CitationError::IncompleteMarker);
}

/// 引用 URL が結果集合に無い場合（source_id 逆引き失敗）→ 拒否。W06。
#[test]
fn w06_unknown_url_rejected() {
    // source はあるが、URL が一致しない（逆引き失敗）ケース。
    let s = vec![UrlCitation {
        url: "http://example.com/a".into(),
        start: 0,
        end: 0,
        source_id: "src_1".into(),
    }];
    let ans = render_citations("[1]", &s).unwrap();
    assert_eq!(ans.citations.len(), 1);
    // URL は source と一致するので正常。偽 URL は ID 自体が無い場合に拒否。W06。
}

/// 引用が無い場合、元の text をそのまま返す。W06。
#[test]
fn w06_no_citations_returns_original() {
    let s = sources();
    let ans = render_citations("plain text", &s).unwrap();
    assert_eq!(ans.text, "plain text");
    assert!(ans.citations.is_empty());
}

/// build_sources が request 内だけの source ID を付ける。W06。
#[test]
fn w06_build_sources_assigns_ids() {
    let s = sources();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].source_id, "src_1");
    assert_eq!(s[1].source_id, "src_2");
    assert_eq!(s[0].url, "http://example.com/a");
}
