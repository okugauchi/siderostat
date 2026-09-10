//! W02 — Responses request validation・通常会話変換。受入 matrix。
//!
//! 本ファイルは W02 カードの target_commands にある `v040_responses_request.rs`
//! に対応する。公開 API（`siderostat::websearch::ResponsesRequest` /
//! `ValidatedResponseRequest` / `RequestError`）を介して受入 case を検証する。
//! 本番 wire 型 + validate を直接駆動し、実プロセス・実検索通信は行わない
//! （dry-run / fake 境界）。
//!
//! 受入 case（全て必須）:
//! - 入力: system/developer/user → 順序保持
//! - 入力: cached/open_page/filter → 400
//! - 入力: oversize chunked body → 413
//! - 入力: 検索なし → 検索通信0
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。

use siderostat::websearch::RequestError;
use siderostat::websearch::request::{MAX_BODY_BYTES, check_body_size};
use siderostat::websearch::wire::ResponsesRequest;

fn parse(json: &str) -> ResponsesRequest {
    serde_json::from_str(json).expect("parse request")
}

/// 受入 case 1: system/developer/user → 順序保持。
/// instructions → 先頭 system、input items → 順序保持で Chat messages へ。
#[test]
fn w02_system_developer_user_preserves_order() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "instructions": "You are a helpful assistant.",
      "input": [
        {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev instructions"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi there"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what is siderostat?"}]}
      ],
      "stream": true
    }"#;
    let req = parse(json);
    let v = req.validate().expect("valid normal conversation");

    // 順序保持: system（instructions）→ developer → user → assistant → user。
    let roles: Vec<&str> = v.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "developer", "user", "assistant", "user"],
        "order must be preserved: {roles:?}"
    );
    assert_eq!(v.messages[0].content, "You are a helpful assistant.");
    assert_eq!(v.messages[1].content, "dev instructions");
    assert_eq!(v.messages[2].content, "hello");
    assert_eq!(v.messages[3].content, "hi there");
    assert_eq!(v.messages[4].content, "what is siderostat?");
    assert_eq!(v.model, "gpt-4.1");
    assert!(v.stream);
}

/// 受入 case 1b: input が文字列の場合は単一 user message。
#[test]
fn w02_string_input_becomes_user_message() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "just a string prompt"
    }"#;
    let req = parse(json);
    let v = req.validate().expect("valid string input");
    assert_eq!(v.messages.len(), 1);
    assert_eq!(v.messages[0].role, "user");
    assert_eq!(v.messages[0].content, "just a string prompt");
    // 検索なし → 検索通信0。
    assert!(v.search.is_none());
}

/// 受入 case 2: cached → 400（必須検索だが live でない）。Codex 既定の cached
/// tool 宣言自体は受信可能（A03）だが、tool_choice 必須では実行できない。
#[test]
fn w02_cached_required_search_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "search something",
      "tools": [
        {"type": "web_search", "external_web_access": false}
      ],
      "tool_choice": "required"
    }"#;
    let req = parse(json);
    let err = req
        .validate()
        .expect_err("cached required search must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2b: cached tool 宣言（auto）は受信可能 → 検索通信0（A03）。
#[test]
fn w02_cached_tool_declaration_is_receivable() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "normal question",
      "tools": [
        {"type": "web_search", "external_web_access": false}
      ],
      "tool_choice": "auto"
    }"#;
    let req = parse(json);
    let v = req.validate().expect("cached tool declaration receivable");
    // live でない → 検索通信0。
    assert!(v.search.is_none());
}

/// 受入 case 2c: open_page 履歴 → 400（MVP unsupported）。
#[test]
fn w02_open_page_history_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": [
        {"type": "web_search_call", "id": "ws_1", "status": "completed",
         "action": {"type": "open_page", "url": "http://example.com"}}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("open_page must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2d: filter → 400（MVP unsupported）。
#[test]
fn w02_filter_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "search restricted",
      "tools": [
        {"type": "web_search", "external_web_access": true,
         "filters": {"allowed_domains": ["example.com"]}}
      ],
      "tool_choice": "auto"
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("filter must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2e: indexed web access → 400（MVP unsupported）。
#[test]
fn w02_indexed_search_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "search indexed",
      "tools": [
        {"type": "web_search", "external_web_access": true, "indexed_web_access": true}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("indexed search must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2f: image search content type → 400（MVP unsupported）。
#[test]
fn w02_image_search_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "search images",
      "tools": [
        {"type": "web_search", "external_web_access": true,
         "search_content_types": ["text", "image"]}
      ]
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("image search must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2g: store=true → 400（stateless bridge）。
#[test]
fn w02_store_true_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "store": true
    }"#;
    let req = parse(json);
    let err = req.validate().expect_err("store=true must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2h: previous_response_id → 400（stateless bridge）。
#[test]
fn w02_previous_response_id_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "previous_response_id": "resp_123"
    }"#;
    let req = parse(json);
    let err = req
        .validate()
        .expect_err("previous_response_id must be 400");
    assert!(matches!(err, RequestError::BadRequest(_)));
}

/// 受入 case 2i: unknown field → 400（deny_unknown_fields、黙って捨てない）。
#[test]
fn w02_unknown_field_is_400() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "hello",
      "some_future_field": "must not be silently dropped"
    }"#;
    assert!(serde_json::from_str::<ResponsesRequest>(json).is_err());
}

/// 受入 case 3: oversize chunked body → 413。
#[test]
fn w02_oversize_body_is_413() {
    let body = "x".repeat(MAX_BODY_BYTES + 1);
    let err = check_body_size(body.len()).expect_err("oversize body must be 413");
    assert!(matches!(err, RequestError::PayloadTooLarge(_)));
}

/// body が上限ちょうどの場合は許容。
#[test]
fn w02_body_at_limit_is_ok() {
    let body = "x".repeat(MAX_BODY_BYTES);
    check_body_size(body.len()).expect("body at limit is ok");
}

/// 受入 case 4: 検索なし → 検索通信0。
/// tools 無し / auto が検索を選ばない場合、search は None（検索通信しない）。
#[test]
fn w02_no_search_means_no_search_traffic() {
    // tools 無し。
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "plain question",
      "tool_choice": "auto"
    }"#;
    let v = parse(json)
        .validate()
        .expect("no tools is a normal conversation");
    assert!(v.search.is_none(), "no tools → no search traffic");

    // tools は web_search だが tool_choice=none（検索しない）。
    let json2 = r#"
    {
      "model": "gpt-4.1",
      "input": "plain question",
      "tools": [
        {"type": "web_search", "external_web_access": true}
      ],
      "tool_choice": "none"
    }"#;
    let v2 = parse(json2)
        .validate()
        .expect("tool_choice none is normal conversation");
    assert!(v2.search.is_none(), "tool_choice none → no search traffic");
}

/// live web_search + auto → 検索はモデル判断（search は live で有効）。
#[test]
fn w02_live_search_is_detected() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "latest news",
      "tools": [
        {"type": "web_search", "external_web_access": true}
      ],
      "tool_choice": "auto"
    }"#;
    let v = parse(json)
        .validate()
        .expect("live search request is valid");
    let s = v.search.expect("live search should be detected");
    assert!(s.live);
    assert!(!s.required);
    assert_eq!(s.query_max_chars, 1024);
}

/// client function tool は実行せず通常会話として保持（W03 で往復変換）。
#[test]
fn w02_function_tool_is_kept_without_execution() {
    let json = r#"
    {
      "model": "gpt-4.1",
      "input": "call my function",
      "tools": [
        {"type": "function", "name": "my_tool",
         "description": "a client tool", "parameters": {"type": "object"}}
      ],
      "tool_choice": "auto"
    }"#;
    let v = parse(json).validate().expect("function tool is acceptable");
    // 検索 tool ではない → 検索通信0。
    assert!(v.search.is_none());
}
