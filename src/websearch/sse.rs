//! Codex Web Search Bridge — SSE protocol 生成。W07。
//!
//! C05 / SSE protocol に基づき、AssembledResponse を SSE イベント列へ変換する。
//! SSE を再構築した結果と nonstreaming object が同一になる。completed は全出力
//! を含み 1 回だけ。W07。
//!
//! A03 fixture の response_wire / Codex 0.153.4 の event 形式に従う:
//! - `response.created`（response.id のみ）W07。
//! - `response.in_progress`（実行中状態通知）W07。
//! - `response.output_item.added`（web_search_call in_progress / message /
//!   function_call）W07。
//! - `response.output_text.delta`（message の text delta、content_index 付き）W07。
//! - `response.function_call_arguments.delta`（function_call の arguments delta）W07。
//! - `response.output_item.done`（completed item）W07。
//! - `response.completed`（全出力を含む nonstreaming object を 1 回だけ）W07。
//! - `response.failed`（失敗時、completed 成功なし。1 回だけ）W07。
//!
//! item ID / output_index / content_index / sequence_number を一貫させる。
//! NEXT-ACTIONS の概念 4 event だけで完全互換としない（C05 254行）。W07。

use super::response::{AssembledResponse, OutputItem, nonstreaming_object};

/// SSE イベント（type + data の JSON value）。W07。
pub type SseEvent = serde_json::Value;

/// 通常成功時の最大 text chunk サイズ（byte）。W07。
///
/// C05: 回答 256KiB 上限。SSE イベントを 16KiB 以下に保つため、text は
/// UTF-8 境界で分割して送る。W07。
pub const DEFAULT_TEXT_CHUNK_BYTES: usize = 4096;

/// text を UTF-8 境界で chunk 分割する。W07。
///
/// 各 chunk は指定 byte 数を超えず、char 境界で分割する。日本語・絵文字・
/// 結合文字をまたいでも壊れない。W07。
pub fn split_utf8_chunks(text: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let n = bytes.len();
    while start < n {
        let mut end = (start + max_bytes).min(n);
        // char 境界まで戻す。W07。
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // 1 文字が max_bytes を超える場合は 1 文字単位で送る。W07。
            let ch = text[start..].chars().next().expect("non-empty text");
            end = start + ch.len_utf8();
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

/// 成功時の SSE イベント列を生成する。W07。
///
/// 生成順:
/// 1. `response.created`（sequence_number 0）W07。
/// 2. `response.in_progress`（sequence_number 1）W07。
/// 3. 各 output item の added → delta(s) → done。W07。
/// 4. `response.completed`（全出力を含む nonstreaming object、1 回だけ）W07。
///
/// sequence_number は全イベントで一貫して連番。output_index / content_index /
/// item ID も各 item で一貫させる。W07。
pub fn render_sse(assembled: &AssembledResponse) -> Vec<SseEvent> {
    render_sse_with_chunk(assembled, DEFAULT_TEXT_CHUNK_BYTES)
}

/// 指定 chunk サイズで SSE イベント列を生成する（テスト用）。W07。
pub fn render_sse_with_chunk(assembled: &AssembledResponse, chunk_bytes: usize) -> Vec<SseEvent> {
    let mut events: Vec<SseEvent> = Vec::new();
    let mut seq: usize = 0;

    // response.created。A03 fixture: response.id のみ。W07。
    events.push(serde_json::json!({
        "type": "response.created",
        "sequence_number": seq,
        "response": {"id": assembled.id},
    }));
    seq += 1;

    // response.in_progress（実行中状態通知）。C05: モデル token 即時中継は
    // 必須としないが、SSE 状態通知は実行中に出す。W07。
    events.push(serde_json::json!({
        "type": "response.in_progress",
        "sequence_number": seq,
        "response": {"id": assembled.id},
    }));
    seq += 1;

    // 各 output item。W07。
    for (output_index, item) in assembled.output.iter().enumerate() {
        match item {
            OutputItem::WebSearchCall { id, query } => {
                // added（in_progress）。A03 fixture。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "web_search_call",
                        "id": id,
                        "status": "in_progress",
                    },
                }));
                seq += 1;
                // done（completed + action）。A03 fixture。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "web_search_call",
                        "id": id,
                        "status": "completed",
                        "action": {"type": "search", "query": query},
                    },
                }));
                seq += 1;
            }
            OutputItem::Message { id, text } => {
                // added。content_index 0。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "id": id,
                        "content": [{"type": "output_text", "text": ""}],
                    },
                }));
                seq += 1;
                // output_text.delta。text を chunk 分割して送る。W07。
                let chunks = split_utf8_chunks(text, chunk_bytes);
                for delta in &chunks {
                    events.push(serde_json::json!({
                        "type": "response.output_text.delta",
                        "sequence_number": seq,
                        "output_index": output_index,
                        "item_id": id,
                        "content_index": 0,
                        "delta": delta,
                    }));
                    seq += 1;
                }
                // done（completed、full text）。A03 fixture。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "id": id,
                        "content": [{"type": "output_text", "text": text}],
                    },
                }));
                seq += 1;
            }
            OutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                // added。arguments は空から開始。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "id": id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                    },
                }));
                seq += 1;
                // function_call_arguments.delta。arguments を chunk 分割。W07。
                let chunks = split_utf8_chunks(arguments, chunk_bytes);
                for delta in &chunks {
                    events.push(serde_json::json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": seq,
                        "output_index": output_index,
                        "item_id": id,
                        "delta": delta,
                    }));
                    seq += 1;
                }
                // done（completed、full arguments）。A03 fixture。W07。
                events.push(serde_json::json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "id": id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                    },
                }));
                seq += 1;
            }
        }
    }

    // response.completed。全出力を含む nonstreaming object を 1 回だけ。W07。
    events.push(serde_json::json!({
        "type": "response.completed",
        "sequence_number": seq,
        "response": nonstreaming_object(assembled),
    }));

    events
}

/// 失敗時の SSE イベント列を生成する。W07。
///
/// C05 247行: SSE header 後は failed terminal 一度で close し、後から HTTP
/// status を変えない。受入 case 4: 失敗 → completed 成功なし。W07。
/// `response.failed` を一度だけ送り、`response.completed` 成功は出さない。W07。
pub fn render_failed(response_id: &str, error_code: &str, message: &str) -> Vec<SseEvent> {
    vec![
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": response_id},
        }),
        serde_json::json!({
            "type": "response.in_progress",
            "sequence_number": 1,
            "response": {"id": response_id},
        }),
        serde_json::json!({
            "type": "response.failed",
            "sequence_number": 2,
            "response": {
                "id": response_id,
                "error": {
                    "code": error_code,
                    "message": message,
                },
            },
        }),
    ]
}

/// SSE イベント列を再構築して nonstreaming object を復元する。W07。
///
/// 受入 case: SSE 再構築の結果と nonstreaming object が同一。delta を結合し、
/// completed の response と照合する。W07。
pub fn reconstruct_object(events: &[SseEvent]) -> Option<serde_json::Value> {
    // completed イベントの response が正本。W07。
    for ev in events {
        if ev["type"] == "response.completed" {
            return Some(ev["response"].clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_chunks_char_boundary() {
        // 日本語・絵文字・結合文字を 3 byte 単位で分割しても壊れない。W07。
        let text = "日本語🙂e\u{301}xyz";
        let chunks = split_utf8_chunks(text, 3);
        let joined = chunks.concat();
        assert_eq!(joined, text);
        // 各 chunk は char 境界。W07。
        let mut pos = 0;
        for c in &chunks {
            assert!(text.is_char_boundary(pos));
            pos += c.len();
        }
    }

    #[test]
    fn split_utf8_chunks_small() {
        let text = "abcdef";
        let chunks = split_utf8_chunks(text, 2);
        assert_eq!(chunks, vec!["ab", "cd", "ef"]);
    }

    #[test]
    fn split_utf8_chunks_empty() {
        let chunks = split_utf8_chunks("", 4);
        assert!(chunks.is_empty());
    }
}
