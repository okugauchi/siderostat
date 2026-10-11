//! W03 — client function と履歴の往復変換。受入 matrix。
//!
//! 本ファイルは W03 カードの target_commands にある `v040_responses_history.rs`
//! に対応する。公開 API（`siderostat::websearch::ResponsesRequest` /
//! `ValidatedResponseRequest` / `HistoryAdapter` / `RequestError` /
//! `INTERNAL_SEARCH_TOOL_NAME`）を介して受入 case を検証する。本番 wire 型 +
//! HistoryAdapter を直接駆動し、実プロセス・実検索通信は行わない（dry-run /
//! fake 境界）。
//!
//! 受入 case（全て必須）:
//! - 入力: search→client tool→output→answer → 対応ID維持
//! - 入力: unknown call_id → 400
//! - 入力: encrypted reasoning/previous_response_id → wire profile で非対応なら明示拒否
//! - 入力: tool name 衝突 → 400
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。

use siderostat::websearch::request::INTERNAL_SEARCH_TOOL_NAME;
use siderostat::websearch::wire::ResponsesRequest;
use siderostat::websearch::{HistoryAdapter, RequestError};

fn parse(json: &str) -> ResponsesRequest {
    serde_json::from_str(json).expect("parse request")
}

/// 受入 case 1: search→client tool→output→answer → 対応ID維持。W03。。
///
/// 履歴: web_search_call（検索）→ function_call（client tool, call_id=X）→
/// function_call_output（call_id=X）→ assistant answer。変換後、tool_calls の
/// call_id と tool message の tool_call_id が X で一致し、role/順序が維持される。
#[test]
fn w03_search_client_tool_output_answer_keeps_id() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "web_search_call", "id": "ws_1", "status": "completed",
         "action": {"type": "search", "query": "latest rust news"}},
        {"type": "function_call", "id": "fc_1", "name": "run_shell",
         "arguments": "{\"cmd\":\"echo hi\"}", "call_id": "call_abc"},
        {"type": "function_call_output", "call_id": "call_abc",
         "output": "hi"},
        {"type": "message", "role": "assistant",
         "content": [{"type": "output_text", "text": "Done."}]}
      ]
    }"#;
    let req = parse(json);
    let v = req.validate().expect("round-trip history is valid");

    // 順序: assistant(web search) → assistant(tool call) → tool(output) → assistant(answer)。
    let roles: Vec<&str> = v.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["assistant", "assistant", "tool", "assistant"],
        "role order must be preserved: {roles:?}"
    );

    // web_search_call は検索履歴として保持（再検索しない）。
    assert_eq!(v.messages[0].content, "[web search performed]");

    // function_call → tool_calls。call_id を保持。
    assert_eq!(v.messages[1].tool_calls.len(), 1);
    let tc = &v.messages[1].tool_calls[0];
    assert_eq!(tc.id, "call_abc", "tool_call id must be preserved");
    assert_eq!(tc.name, "run_shell");
    assert_eq!(tc.arguments, r#"{"cmd":"echo hi"}"#);

    // function_call_output → tool role。tool_call_id が call_abc と一致。。
    assert_eq!(v.messages[2].role, "tool");
    assert_eq!(v.messages[2].tool_call_id.as_deref(), Some("call_abc"));
    assert_eq!(v.messages[2].content, "hi");

    // assistant answer が保持される。
    assert_eq!(v.messages[3].content, "Done.");
}

/// 受入 case 1b: custom tool call の round-trip も ID 維持。W03。。
#[test]
fn w03_custom_tool_output_keeps_id() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "custom_tool_call", "name": "my_tool",
         "input": "{\"a\":1}", "call_id": "call_xyz"},
        {"type": "custom_tool_call_output", "call_id": "call_xyz",
         "output": "result payload"}
      ]
    }"#;
    let req = parse(json);
    let v = req.validate().expect("custom tool round-trip is valid");
    assert_eq!(v.messages.len(), 2);
    assert_eq!(v.messages[0].role, "assistant");
    assert_eq!(v.messages[0].tool_calls[0].id, "call_xyz");
    assert_eq!(v.messages[0].tool_calls[0].name, "my_tool");
    assert_eq!(v.messages[1].role, "tool");
    assert_eq!(v.messages[1].tool_call_id.as_deref(), Some("call_xyz"));
    assert_eq!(v.messages[1].content, "result payload");
}

/// 受入 case 2: unknown call_id → 400。W03。。
///
/// function_call_output が先行 function_call の無い call_id を参照 → 400。
#[test]
fn w03_unknown_call_id_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "function_call_output", "call_id": "call_unknown", "output": "x"}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("unknown call_id must be 400");
    match err {
        RequestError::BadRequest(msg) => assert!(msg.contains("unknown"), "{msg}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

/// 受入 case 2b: unknown custom tool call_id → 400。W03。。
#[test]
fn w03_unknown_custom_call_id_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "custom_tool_call_output", "call_id": "call_nope", "output": "x"}
      ]
    }"#;
    let req = parse(json);
    let err = req
        .validate()
        .expect_err("unknown custom call_id must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2c: output が先行 call より先に来る順序不正 → 400。W03。。
#[test]
fn w03_output_before_call_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "function_call_output", "call_id": "call_late", "output": "x"},
        {"type": "function_call", "name": "t", "arguments": "{}", "call_id": "call_late"}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("output before call must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 3a: encrypted reasoning → 400（wire profile で非対応）。W03。。
#[test]
fn w03_encrypted_reasoning_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "reasoning", "summary": [{"text": "thinking"}],
         "encrypted_content": "encrypted-blob"}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("encrypted reasoning must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 3b: previous_response_id → 400（stateless bridge）。W02 継続。W03。。
#[test]
fn w03_previous_response_id_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "previous_response_id": "resp_1"
    }"#;
    let req = parse(json);
    let err = req
        .validate()
        .expect_err("previous_response_id must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 3c: 平文 reasoning summary は履歴として無視（DS4 には送らない）。W03。。
#[test]
fn w03_plaintext_reasoning_is_skipped() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "reasoning", "summary": [{"text": "thinking out loud"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
      ]
    }"#;
    let req = parse(json);
    let v = req.validate().expect("plaintext reasoning is skipped");
    // reasoning は DS4 へ送らない。user message のみ。
    assert_eq!(v.messages.len(), 1);
    assert_eq!(v.messages[0].role, "user");
    assert_eq!(v.messages[0].content, "hi");
}

/// 受入 case 4: tool name 衝突 → 400。W03。。
///
/// user tool が内部検索 tool 名（siderostat_web_search）と同名 → 400。。
#[test]
fn w03_tool_name_conflict_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "tools": [
        {"type": "function", "name": "siderostat_web_search",
         "description": "user tool colliding with reserved name"}
      ]
    }"#;
    let req = parse(json);
    let err = req
        .validate()
        .expect_err("reserved tool name collision must be 400");
    match err {
        RequestError::BadRequest(msg) => assert!(msg.contains("reserved"), "{msg}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

/// 受入 case 4b: 内部検索 tool 名定数が C05 と一致する。W03。。
#[test]
fn w03_internal_search_tool_name_matches_contract() {
    assert_eq!(INTERNAL_SEARCH_TOOL_NAME, "siderostat_web_search");
}

/// 受入 case 4c: 非衝突 tool 名は許容される。W03。。
#[test]
fn w03_non_conflicting_tool_name_is_ok() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "tools": [
        {"type": "function", "name": "my_shell_tool"}
      ]
    }"#;
    let v = parse(json)
        .validate()
        .expect("non-conflicting tool name is ok");
    assert!(v.search.is_none());
}

/// HistoryAdapter を直接駆動して round-trip を検証する。W03。。
#[test]
fn w03_history_adapter_direct() {
    let items = vec![
        siderostat::websearch::wire::ResponseInputItem::FunctionCall {
            id: Some("fc_9".into()),
            name: "run_shell".into(),
            arguments: "{}".into(),
            call_id: "call_9".into(),
        },
        siderostat::websearch::wire::ResponseInputItem::FunctionCallOutput {
            name: Some("run_shell".into()),
            call_id: "call_9".into(),
            output: serde_json::json!("ok"),
        },
    ];
    let messages = HistoryAdapter::to_chat_messages(&items).expect("adapter round-trip");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "assistant");
    assert_eq!(messages[0].tool_calls[0].id, "call_9");
    assert_eq!(messages[1].role, "tool");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_9"));
    assert_eq!(messages[1].content, "ok");
}
