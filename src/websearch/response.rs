//! Codex Web Search Bridge — Responses object の組み立て。W07。
//!
//! C05 / ResponseAssembler に基づき、engine outcome（検索 query・最終 text・
//! client function call）を Responses の output item 列へ組み立てる。SSE 再構築
//! の結果と nonstreaming object が同一になるよう、item ID / output_index /
//! content_index / sequence_number を一貫させる。W07。
//!
//! A03 fixture の response_wire に従う:
//! - web_search_call item: `{type: web_search_call, id, status: completed,
//!   action: {type: search, query}}`
//! - message item: `{type: message, role: assistant, id, content:
//!   [{type: output_text, text}]}`
//! - function_call item: `{type: function_call, call_id, name, arguments}`
//!
//! 受入 case（全て必須）:
//! - 入力: search2件+answer → 同ID/同順序
//! - 入力: client function → arguments delta/done
//! - 入力: 複数text chunk → 最終text一致
//! - 入力: 失敗 → completed成功なし

use super::engine::EngineOutcome;
use super::request::ChatToolCall;

/// 出力 item の種別。A03 response_wire に対応。W07。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputItem {
    /// web_search_call。status=completed、action search を保持する。W07。
    WebSearchCall {
        /// item ID（一貫）。W07。
        id: String,
        /// 検索 query。W07。
        query: String,
    },
    /// assistant message。最終 text を保持する。W07。
    Message {
        /// item ID（一貫）。W07。
        id: String,
        /// 最終回答 text（citation 済み）。W07。
        text: String,
    },
    /// function_call。client tool call を保持する。W07。
    FunctionCall {
        /// item ID（一貫）。W07。
        id: String,
        /// call_id（W03 の往復変換で保持）。W07。
        call_id: String,
        /// tool 名。W07。
        name: String,
        /// arguments（JSON 文字列）。W07。
        arguments: String,
    },
}

/// 組み立て済み Responses object。W07。
///
/// nonstreaming object の正本。SSE 再構築でこれと同一になる。W07。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledResponse {
    /// response ID。W07。
    pub id: String,
    /// 出力 item 列（順序保持）。W07。
    pub output: Vec<OutputItem>,
    /// 消費した model turn 数。usage 計算用。W07。
    pub model_turns: usize,
}

/// Responses object を組み立てる。W07。
///
/// 出力順序:
/// 1. web_search_call items（検索 query 順）。W07。
/// 2. function_call items（client_calls 順）。W07。
/// 3. message item（最終回答 text）。W07。
///
/// 各 item に一貫した ID（`item_0` から連番）を付与する。同一 outcome からは
/// 常に同じ組み立て結果が得られる（決定論的）。W07。
pub fn assemble(outcome: &EngineOutcome, response_id: &str) -> AssembledResponse {
    let mut output: Vec<OutputItem> = Vec::new();
    let mut item_idx: usize = 0;

    // 1. web_search_call items。検索 query 順。W07。
    for query in &outcome.search_queries {
        output.push(OutputItem::WebSearchCall {
            id: format!("item_{item_idx}"),
            query: query.clone(),
        });
        item_idx += 1;
    }

    // 2. function_call items。client_calls 順。W07。
    for call in &outcome.client_calls {
        output.push(client_function_item(&mut item_idx, call));
    }

    // 3. message item。最終回答 text。W07。
    output.push(OutputItem::Message {
        id: format!("item_{item_idx}"),
        text: outcome.text.clone(),
    });

    AssembledResponse {
        id: response_id.to_string(),
        output,
        model_turns: outcome.model_turns,
    }
}

/// client tool call を function_call item へ変換する。W07。
fn client_function_item(item_idx: &mut usize, call: &ChatToolCall) -> OutputItem {
    let item = OutputItem::FunctionCall {
        id: format!("item_{item_idx}"),
        call_id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    };
    *item_idx += 1;
    item
}

/// 出力 item を nonstreaming object の JSON value へ変換する。W07。
pub fn output_item_to_json(item: &OutputItem) -> serde_json::Value {
    match item {
        OutputItem::WebSearchCall { id, query } => serde_json::json!({
            "type": "web_search_call",
            "id": id,
            "status": "completed",
            "action": {"type": "search", "query": query}
        }),
        OutputItem::Message { id, text } => serde_json::json!({
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}]
        }),
        OutputItem::FunctionCall {
            id,
            call_id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "function_call",
            "id": id,
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        }),
    }
}

/// nonstreaming object（completed の response）を生成する。W07。
///
/// `response.completed` の response フィールドと同一になる。SSE 再構築の正本。W07。
pub fn nonstreaming_object(assembled: &AssembledResponse) -> serde_json::Value {
    let output: Vec<serde_json::Value> = assembled.output.iter().map(output_item_to_json).collect();
    serde_json::json!({
        "id": assembled.id,
        "object": "response",
        "output": output,
        "usage": {
            "input_tokens": 0,
            "input_tokens_details": null,
            "output_tokens": 0,
            "output_tokens_details": null,
            "total_tokens": 0,
        },
        "model_turns": assembled.model_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websearch::backend::SearchResult;
    use url::Url;

    fn outcome() -> EngineOutcome {
        EngineOutcome {
            text: "final answer".into(),
            search_results: vec![SearchResult {
                title: "r1".into(),
                url: Url::parse("http://example.com/1").unwrap(),
                snippet: "s1".into(),
            }],
            search_queries: vec!["q1".into(), "q2".into()],
            model_turns: 3,
            client_calls: vec![ChatToolCall {
                id: "call_1".into(),
                name: "my_tool".into(),
                arguments: "{}".into(),
            }],
        }
    }

    #[test]
    fn assemble_produces_items_in_order() {
        let o = outcome();
        let a = assemble(&o, "resp_1");
        assert_eq!(a.id, "resp_1");
        assert_eq!(a.output.len(), 4);
        // search 2 + client function 1 + message 1。
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
            OutputItem::FunctionCall { call_id, name, .. }
                if call_id == "call_1" && name == "my_tool"
        ));
        assert!(matches!(
            &a.output[3],
            OutputItem::Message { text, .. } if text == "final answer"
        ));
    }

    #[test]
    fn item_ids_are_consistent_sequential() {
        let o = outcome();
        let a = assemble(&o, "resp_1");
        let ids: Vec<&str> = a
            .output
            .iter()
            .map(|item| match item {
                OutputItem::WebSearchCall { id, .. }
                | OutputItem::Message { id, .. }
                | OutputItem::FunctionCall { id, .. } => id.as_str(),
            })
            .collect();
        assert_eq!(ids, vec!["item_0", "item_1", "item_2", "item_3"]);
    }

    #[test]
    fn deterministic_for_same_outcome() {
        let o = outcome();
        let a1 = assemble(&o, "resp_1");
        let a2 = assemble(&o, "resp_1");
        assert_eq!(a1, a2);
    }

    #[test]
    fn nonstreaming_object_matches_items() {
        let o = outcome();
        let a = assemble(&o, "resp_1");
        let obj = nonstreaming_object(&a);
        assert_eq!(obj["id"], "resp_1");
        assert_eq!(obj["object"], "response");
        assert_eq!(obj["output"].as_array().unwrap().len(), 4);
        assert_eq!(obj["output"][0]["type"], "web_search_call");
        assert_eq!(obj["output"][0]["action"]["query"], "q1");
        assert_eq!(obj["output"][3]["type"], "message");
        assert_eq!(obj["output"][3]["content"][0]["text"], "final answer");
    }
}
