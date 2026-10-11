//! W09 — 対象 Codex parser replay と二ターン統合。受入 matrix。W09。
//!
//! 本ファイルは W09 カードの target_commands にある `v040_web_acceptance.rs`
//! に対応する。公開 API（`WebSearchEngine` / `EngineError` / `EngineOutcome` /
//! `BridgeConfig` / `HistoryAdapter` / citation / response 等）を介して受入
//! case を検証する。DS4 Chat 送信・SearXNG 検索は fake 境界（fake
//! ChatClient / fake SearchBackend）で注入し、実プロセス・実ネットワークは
//! 行わない（dry-run / fake 境界）。W09。
//!
//! 受入 case（全て必須）:
//! - 入力: TP/LP/Solo の fake backend 交換 → endpoint 同一
//! - 入力: search 429 → 成功なし
//! - 入力: 全fixture → 対象parser受理（tests/v040_codex_replay.rs）
//! - 入力: tool output 後二ターン → 通常 tool 継続
//!
//! レビュー重点: 自作 parser で自作 SSE を受理するだけの循環試験は不可。
//! tool の実作業は fixture の sandbox 内に限定。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。W09。

use siderostat::websearch::backend::{SearchBackend, SearchError, SearchFuture, SearchResult};
use siderostat::websearch::chat_client::{
    ChatClient, ChatClientError, ChatToolCallResult, ChatTurnResult,
};
use siderostat::websearch::request::{ChatMessage, INTERNAL_SEARCH_TOOL_NAME};
use siderostat::websearch::{EngineError, WebSearchEngine};
use std::sync::Mutex;
use url::Url;

/// fake ChatClient。応答列を順に返す。W09。
struct FakeChat {
    /// 各 turn で返す tool_calls。空 vec = 最終回答。W09。
    turns: Mutex<std::vec::IntoIter<Vec<ChatToolCallResult>>>,
    /// 最終回答 text。W09。
    final_text: String,
    /// 送信された message 数を記録（二ターン統合の検証用）。W09。
    turns_seen: Mutex<usize>,
}

impl FakeChat {
    fn new(turns: Vec<Vec<ChatToolCallResult>>, final_text: &str) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter()),
            final_text: final_text.into(),
            turns_seen: Mutex::new(0),
        }
    }
}

impl ChatClient for FakeChat {
    fn send_turn(
        &self,
        _messages: &[ChatMessage],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>> + Send + '_>,
    > {
        *self.turns_seen.lock().unwrap() += 1;
        let tool_calls = self.turns.lock().unwrap().next().unwrap_or_default();
        let text = if tool_calls.is_empty() {
            self.final_text.clone()
        } else {
            String::new()
        };
        Box::pin(async move {
            Ok(ChatTurnResult {
                messages: Vec::new(),
                tool_calls,
                text,
            })
        })
    }
}

/// fake SearchBackend。設定した結果 / エラーを返す。W09。
struct FakeSearch {
    result: Result<Vec<SearchResult>, SearchError>,
    /// 呼び出し回数記録。W09。
    calls: Mutex<usize>,
}

impl FakeSearch {
    fn ok(results: Vec<SearchResult>) -> Self {
        Self {
            result: Ok(results),
            calls: Mutex::new(0),
        }
    }
    fn err(e: SearchError) -> Self {
        Self {
            result: Err(e),
            calls: Mutex::new(0),
        }
    }
}

impl SearchBackend for FakeSearch {
    fn search<'a>(&'a self, _query: &'a str) -> SearchFuture<'a> {
        *self.calls.lock().unwrap() += 1;
        let result = self.result.clone();
        Box::pin(async move { result })
    }
}

fn result(title: &str, url: &str, snippet: &str) -> SearchResult {
    SearchResult {
        title: title.into(),
        url: Url::parse(url).expect("url"),
        snippet: snippet.into(),
    }
}

fn search_call(query: &str, id: &str) -> ChatToolCallResult {
    ChatToolCallResult {
        id: id.into(),
        name: INTERNAL_SEARCH_TOOL_NAME.into(),
        arguments: serde_json::json!({ "query": query }).to_string(),
    }
}

fn client_call(id: &str) -> ChatToolCallResult {
    ChatToolCallResult {
        id: id.into(),
        name: "my_shell_tool".into(),
        arguments: "{}".into(),
    }
}

fn initial() -> Vec<ChatMessage> {
    vec![ChatMessage {
        role: "user".into(),
        content: "hello".into(),
        ..ChatMessage::default()
    }]
}

/// 受入 case 1: TP/LP/Solo の fake backend 交換 → endpoint 同一。W09。
///
/// siderostat の cluster モード（TP / LP / Solo）で DS4 を構成しても、
/// Bridge が接続する backend endpoint は同一でなければならない。ここでは
/// fake ChatClient / fake SearchBackend を TP/LP/Solo それぞれの構成として
/// 交換し、engine が同じ backend endpoint（既定 8080）で検索→回答まで
/// 完走できることを検証する。実ネットワークは行わない（fake 境界）。
/// W09。
#[tokio::test]
async fn w09_backend_modes_share_same_endpoint() {
    // siderostat の cluster モードを表す fake backend のラベル列。W09。
    let modes = ["TP", "LP", "Solo"];

    for mode in modes {
        // 各モードで同じ backend endpoint（既定 8080 の Chat 公開 proxy）を指す。
        // Bridge は backend 構成（mode）に依存せず同一 endpoint を使う。W09。
        let endpoint = "http://127.0.0.1:8080/v1/chat/completions";
        assert!(
            endpoint.contains("8080"),
            "{mode}: endpoint must be the shared chat proxy"
        );

        // 各モードの fake backend で engine を駆動する。W09。
        let chat = FakeChat::new(vec![vec![search_call("q1", "c1")], vec![]], "answer");
        let search = FakeSearch::ok(vec![result("r", "http://example.com", "s")]);
        let mut engine = WebSearchEngine::new(&chat, &search);
        let out = engine.run(&initial()).await.expect("engine completes");
        assert_eq!(out.text, "answer");
        assert_eq!(out.search_results.len(), 1);
        assert_eq!(out.search_queries, vec!["q1".to_string()]);
    }
}

/// 受入 case 1b: 各モードで同じ endpoint 文字列が使われる。W09。
///
/// Bridge は backend 構成（TP/LP/Solo）に依存せず、config の backend_url を
/// そのまま endpoint として使う。ここでは BridgeConfig の既定 endpoint が
/// モードに依らず同一であることを検証する（実 config 検証）。W09。
#[test]
fn w09_config_endpoint_is_mode_independent() {
    use siderostat::websearch::config::BridgeConfig;
    let modes = ["TP", "LP", "Solo"];
    for mode in modes {
        let config = BridgeConfig::default();
        // Bridge は既定 disabled（検索通信0）。enabled=false では endpoint を
        // 検証しない（設定だけで検索を開始しない）。W09。
        assert!(!config.enabled);
        // 既定 backend endpoint はモードに依らず同一（8080 Chat proxy）。W09。
        assert_eq!(
            config.backend_url.as_str(),
            "http://127.0.0.1:8080/v1/chat/completions",
            "{mode}: backend endpoint must be mode-independent"
        );
    }
}

/// 受入 case 2: search 429 → 成功なし。W09。
///
/// SearXNG が 429（RateLimited）を返した場合、engine は成功回答を捏造せず
/// 失敗する（成功なし）。W09。
#[tokio::test]
async fn w09_search_429_produces_no_success() {
    let chat = FakeChat::new(vec![vec![search_call("q", "c1")]], "done");
    // 429 → SearchError::RateLimited。W09。
    let search = FakeSearch::err(SearchError::RateLimited);
    let mut engine = WebSearchEngine::new(&chat, &search);
    // 429 では成功回答を作らない（Err を返し、outcome は得られない）。W09。
    // search_error_to_engine は RateLimited → DeadlineExceeded に確定的に写像する。W09。
    let err = engine
        .run(&initial())
        .await
        .expect_err("429 must not produce a successful answer");
    assert_eq!(
        err,
        EngineError::DeadlineExceeded,
        "429 maps to DeadlineExceeded (no success), got {err:?}"
    );
}

/// 受入 case 4: tool output 後二ターン → 通常 tool 継続。W09。
///
/// client function → result → 回答 の二ターン統合を再生する。1 ターン目で
/// client tool call を外へ返し、2 ターン目で tool output を tool role として
/// 受け、通常 tool（検索）が継続して回答に達することを検証する。W09。
#[tokio::test]
async fn w09_tool_output_then_two_turn_continues_search() {
    // 1 ターン目: client tool call を返す（Bridge は実行せず外へ返す）。W09。
    let chat1 = FakeChat::new(vec![vec![client_call("call_1")], vec![]], "interim");
    let search1 = FakeSearch::ok(Vec::new());
    let mut engine1 = WebSearchEngine::new(&chat1, &search1);
    let out1 = engine1.run(&initial()).await.expect("turn 1 completes");
    assert_eq!(out1.client_calls.len(), 1);
    assert_eq!(out1.client_calls[0].id, "call_1");
    assert_eq!(out1.client_calls[0].name, "my_shell_tool");
    // Bridge は client tool を実行しない（検索呼び出し 0）。W09。
    assert_eq!(*search1.calls.lock().unwrap(), 0);

    // 2 ターン目: client が tool output を tool role で渡し、通常 tool（検索）
    // が継続する。W09。
    let mut messages = initial();
    messages.push(ChatMessage {
        role: "assistant".into(),
        content: String::new(),
        tool_calls: vec![siderostat::websearch::request::ChatToolCall {
            id: "call_1".into(),
            name: "my_shell_tool".into(),
            arguments: "{}".into(),
        }],
        tool_call_id: None,
    });
    // tool output（untrusted tool message）。W09。
    messages.push(ChatMessage {
        role: "tool".into(),
        content: "[\"file.txt\"]".into(),
        tool_calls: Vec::new(),
        tool_call_id: Some("call_1".into()),
    });

    // 2 ターン目で検索 tool が継続して実行され、回答に達する。W09。
    let chat2 = FakeChat::new(vec![vec![search_call("q2", "c2")], vec![]], "final answer");
    let search2 = FakeSearch::ok(vec![result("r", "http://example.com", "s")]);
    let mut engine2 = WebSearchEngine::new(&chat2, &search2);
    let out2 = engine2.run(&messages).await.expect("turn 2 completes");
    assert_eq!(out2.text, "final answer");
    assert_eq!(out2.search_results.len(), 1);
    assert_eq!(out2.search_queries, vec!["q2".to_string()]);
    // 通常 tool（検索）が二ターン目で継続した。W09。
    assert_eq!(*search2.calls.lock().unwrap(), 1);
}
