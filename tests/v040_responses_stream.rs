//! W07 — Responses object と SSE event 整合。受入 matrix。W07。
//!
//! 公開 API（`siderostat::websearch::response::{assemble, nonstreaming_object,
//! OutputItem, AssembledResponse}` / `siderostat::websearch::sse::{render_sse,
//! render_sse_with_chunk, render_failed, reconstruct_object,
//! split_utf8_chunks}`）を介して検証する。実プロセス・実検索・SSE 送信は
//! 行わない（dry-run / fake 境界）。W07。
//!
//! 受入 case（全て必須）:
//! - 入力: search2件+answer → 同ID/同順序
//! - 入力: client function → arguments delta/done
//! - 入力: 複数text chunk → 最終text一致
//! - 入力: 失敗 → completed成功なし
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。W07。

use siderostat::websearch::engine::EngineOutcome;
use siderostat::websearch::request::ChatToolCall;
use siderostat::websearch::response::{OutputItem, assemble, nonstreaming_object};
use siderostat::websearch::sse::{
    reconstruct_object, render_failed, render_sse, render_sse_with_chunk, split_utf8_chunks,
};

/// 検索2件 + client function 1件 + 回答 を持つ outcome。W07。
fn outcome() -> EngineOutcome {
    EngineOutcome {
        text: "最終回答 text".into(),
        search_results: Vec::new(),
        search_queries: vec!["q1".into(), "q2".into()],
        model_turns: 3,
        client_calls: vec![ChatToolCall {
            id: "call_1".into(),
            name: "my_shell_tool".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        }],
    }
}

/// 受入 case 1: search2件+answer → 同ID/同順序。W07。
#[test]
fn w07_search_two_and_answer_same_id_order() {
    let o = EngineOutcome {
        search_queries: vec!["q1".into(), "q2".into()],
        client_calls: Vec::new(),
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    // 順序: web_search_call ×2 → message。W07。
    assert_eq!(a.output.len(), 3);
    assert!(matches!(
        &a.output[0],
        OutputItem::WebSearchCall { query, .. } if query == "q1"
    ));
    assert!(matches!(
        &a.output[1],
        OutputItem::WebSearchCall { query, .. } if query == "q2"
    ));
    assert!(matches!(
        &a.output[2],
        OutputItem::Message { text, .. } if text == "最終回答 text"
    ));
    // item ID 一貫。W07。
    let ids: Vec<&str> = a
        .output
        .iter()
        .map(|item| match item {
            OutputItem::WebSearchCall { id, .. }
            | OutputItem::Message { id, .. }
            | OutputItem::FunctionCall { id, .. } => id.as_str(),
        })
        .collect();
    assert_eq!(ids, vec!["item_0", "item_1", "item_2"]);
    // SSE を再構築すると同一。W07。
    let events = render_sse(&a);
    let rebuilt = reconstruct_object(&events).expect("completed event");
    assert_eq!(rebuilt, nonstreaming_object(&a));
}

/// 受入 case 1b: SSE の item 順序が output と一致。W07。
#[test]
fn w07_sse_item_order_matches_output() {
    let o = EngineOutcome {
        search_queries: vec!["q1".into(), "q2".into()],
        client_calls: Vec::new(),
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    let events = render_sse(&a);
    // web_search_call の done が 2 回、message の done が 1 回。W07。
    let done_types: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| e["item"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        done_types,
        vec!["web_search_call", "web_search_call", "message"]
    );
    // web_search_call の query 順序。W07。
    let queries: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .filter(|e| e["item"]["type"] == "web_search_call")
        .map(|e| e["item"]["action"]["query"].as_str().unwrap())
        .collect();
    assert_eq!(queries, vec!["q1", "q2"]);
}

/// 受入 case 2: client function → arguments delta/done。W07。
#[test]
fn w07_client_function_arguments_delta_done() {
    let o = EngineOutcome {
        search_queries: Vec::new(),
        client_calls: vec![ChatToolCall {
            id: "call_1".into(),
            name: "my_shell_tool".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        }],
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    let events = render_sse(&a);
    // function_call の added → arguments delta → done。W07。
    let function_added = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["item"]["type"] == "function_call")
        .expect("function_call added");
    assert_eq!(function_added["item"]["name"], "my_shell_tool");
    assert_eq!(function_added["item"]["call_id"], "call_1");
    // arguments delta が存在。W07。
    let deltas: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    let joined = deltas.concat();
    assert_eq!(joined, "{\"cmd\":\"ls\"}");
    // done は full arguments。W07。
    let done = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call")
        .expect("function_call done");
    assert_eq!(done["item"]["arguments"], "{\"cmd\":\"ls\"}");
    // SSE 再構築と一致。W07。
    let rebuilt = reconstruct_object(&events).expect("completed");
    assert_eq!(rebuilt, nonstreaming_object(&a));
}

/// 受入 case 2b: client function の item ID 一貫。W07。
#[test]
fn w07_client_function_item_id_consistent() {
    let o = EngineOutcome {
        search_queries: Vec::new(),
        client_calls: vec![ChatToolCall {
            id: "call_1".into(),
            name: "my_shell_tool".into(),
            arguments: "{}".into(),
        }],
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    let events = render_sse(&a);
    // added / delta / done の item_id が一致。W07。
    let added_id = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["item"]["type"] == "function_call")
        .map(|e| e["item"]["id"].as_str().unwrap())
        .unwrap();
    let delta_id = events
        .iter()
        .find(|e| e["type"] == "response.function_call_arguments.delta")
        .map(|e| e["item_id"].as_str().unwrap())
        .unwrap();
    let done_id = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call")
        .map(|e| e["item"]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(added_id, delta_id);
    assert_eq!(added_id, done_id);
}

/// 受入 case 3: 複数text chunk → 最終text一致。W07。
#[test]
fn w07_multiple_text_chunks_final_text_matches() {
    let o = EngineOutcome {
        search_queries: Vec::new(),
        client_calls: Vec::new(),
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    // 小さい chunk で複数 delta に分割。W07。
    let events = render_sse_with_chunk(&a, 3);
    let deltas: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    // 複数 chunk。W07。
    assert!(
        deltas.len() > 1,
        "expected multiple deltas, got {}",
        deltas.len()
    );
    let joined = deltas.concat();
    // UTF-8 char 境界で分割しても結合は元 text と一致。W07。
    assert_eq!(joined, "最終回答 text");
    // done の message は full text。W07。
    let done = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "message")
        .expect("message done");
    assert_eq!(done["item"]["content"][0]["text"], "最終回答 text");
    // SSE 再構築と nonstreaming が一致。W07。
    let rebuilt = reconstruct_object(&events).expect("completed");
    assert_eq!(rebuilt, nonstreaming_object(&a));
}

/// 受入 case 3b: text chunk の content_index 一貫。W07。
#[test]
fn w07_text_chunk_content_index_consistent() {
    let o = EngineOutcome {
        search_queries: Vec::new(),
        client_calls: Vec::new(),
        ..outcome()
    };
    let a = assemble(&o, "resp_1");
    let events = render_sse_with_chunk(&a, 3);
    let deltas: Vec<u64> = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["content_index"].as_u64().unwrap())
        .collect();
    // 全 delta が content_index 0 を参照。W07。
    assert!(deltas.iter().all(|&i| i == 0));
    // item_id 一貫。W07。
    let item_ids: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["item_id"].as_str().unwrap())
        .collect();
    let first = item_ids[0];
    assert!(item_ids.iter().all(|&id| id == first));
}

/// 受入 case 4: 失敗 → completed成功なし。W07。
#[test]
fn w07_failure_no_completed_success() {
    let events = render_failed("resp_1", "backend_unavailable", "DS4 503");
    // failed は 1 回だけ。W07。
    let failed = events
        .iter()
        .filter(|e| e["type"] == "response.failed")
        .count();
    assert_eq!(failed, 1);
    // completed 成功なし。W07。
    let completed = events
        .iter()
        .filter(|e| e["type"] == "response.completed")
        .count();
    assert_eq!(completed, 0);
    // created → in_progress → failed の順。W07。
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.in_progress");
    assert_eq!(events[2]["type"], "response.failed");
    // failed の error。W07。
    assert_eq!(
        events[2]["response"]["error"]["code"],
        "backend_unavailable"
    );
    // sequence_number 一貫。W07。
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| e["sequence_number"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

/// 受入 case 1c: completed は全出力を含み 1 回だけ。W07。
#[test]
fn w07_completed_once_with_all_output() {
    let a = assemble(&outcome(), "resp_1");
    let events = render_sse(&a);
    let completed = events
        .iter()
        .filter(|e| e["type"] == "response.completed")
        .count();
    assert_eq!(completed, 1);
    let obj = reconstruct_object(&events).expect("completed");
    // 全出力（web_search_call 2 + function_call 1 + message 1）を含む。W07。
    assert_eq!(obj["output"].as_array().unwrap().len(), 4);
    let types: Vec<&str> = obj["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "web_search_call",
            "web_search_call",
            "function_call",
            "message"
        ]
    );
}

/// sequence_number が全イベントで一貫。W07。
#[test]
fn w07_sequence_numbers_consistent() {
    let a = assemble(&outcome(), "resp_1");
    let events = render_sse(&a);
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| e["sequence_number"].as_u64().unwrap())
        .collect();
    let expected: Vec<u64> = (0..events.len() as u64).collect();
    assert_eq!(seqs, expected);
}

/// split_utf8_chunks が UTF-8 char 境界で分割。W07。
#[test]
fn w07_split_chunks_char_boundary() {
    let text = "日本語🙂e\u{301}xyz";
    let chunks = split_utf8_chunks(text, 3);
    let joined = chunks.concat();
    assert_eq!(joined, text);
    let mut pos = 0;
    for c in &chunks {
        assert!(text.is_char_boundary(pos));
        pos += c.len();
    }
}

/// output_index が各 item で一貫。W07。
#[test]
fn w07_output_index_consistent() {
    let a = assemble(&outcome(), "resp_1");
    let events = render_sse(&a);
    // added の output_index が item 順。W07。
    let added_indices: Vec<u64> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.added")
        .map(|e| e["output_index"].as_u64().unwrap())
        .collect();
    assert_eq!(added_indices, vec![0, 1, 2, 3]);
}
