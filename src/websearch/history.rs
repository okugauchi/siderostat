//! Codex Web Search Bridge — Responses function call と履歴の往復変換。
//!
//! C05 に基づき、Responses の function call を Chat tool_calls へ、その結果を
//! tool role へ変換する。client tool は Bridge が実行せず、Responses
//! function_call として返し、次ターンの call_id を照合する。履歴の role /
//! call ID を失わない。内部検索 tool 名（`siderostat_web_search`）を予約し、
//! user tool との衝突を拒否する。
//!
//! 契約: CONTRACTS.md C05 / HistoryAdapter・client tool mapping。W03。
//!
//! 受入 case（全て必須）:
//! - 入力: search→client tool→output→answer → 対応ID維持
//! - 入力: unknown call_id → 400
//! - 入力: encrypted reasoning/previous_response_id → wire profile で非対応なら明示拒否
//! - 入力: tool name 衝突 → 400

use super::request::{ChatMessage, ChatToolCall, INTERNAL_SEARCH_TOOL_NAME, RequestError};
use super::wire::{ContentItem, ResponseInputItem, WebSearchAction};

/// 履歴の往復変換を行う adapter。W03。
///
/// - Responses function call（FunctionCall / CustomToolCall）→ Chat assistant
///   message の tool_calls（call_id 保持）。
/// - function_call_output / custom_tool_call_output → Chat tool message
///   （tool_call_id 保持）。call_id が先行 call と照合できない場合は 400。
/// - web_search_call → 履歴として保持（再検索しない）。open_page /
///   find_in_page → 400。
/// - encrypted reasoning → 400（wire profile で非対応）。
pub struct HistoryAdapter;

impl HistoryAdapter {
    /// input item 配列を Chat messages へ順序保持で変換する。
    ///
    /// `call_id` の照合状態（発行済み call）を追跡する。output item の
    /// call_id が発行済みでない場合は `unknown call_id` として 400 を返す。
    pub fn to_chat_messages(items: &[ResponseInputItem]) -> Result<Vec<ChatMessage>, RequestError> {
        let mut messages = Vec::new();
        // 発行済み call_id の集合。output がこの集合に無い call_id を
        // 参照したら unknown call_id → 400。
        let mut issued_calls: Vec<String> = Vec::new();

        for item in items {
            match item {
                ResponseInputItem::Message { role, content } => {
                    let text = message_text(content)?;
                    messages.push(ChatMessage {
                        role: role.clone(),
                        content: text,
                        ..ChatMessage::default()
                    });
                }
                ResponseInputItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    // client tool は実行しない。tool_calls として保持。
                    // call_id を発行済みとして記録（次ターン output 照合用）。
                    issued_calls.push(call_id.clone());
                    messages.push(ChatMessage {
                        role: "assistant".into(),
                        content: String::new(),
                        tool_calls: vec![ChatToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }],
                        tool_call_id: None,
                    });
                }
                ResponseInputItem::CustomToolCall {
                    name,
                    input,
                    call_id,
                    ..
                } => {
                    issued_calls.push(call_id.clone());
                    messages.push(ChatMessage {
                        role: "assistant".into(),
                        content: String::new(),
                        tool_calls: vec![ChatToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            arguments: input.clone(),
                        }],
                        tool_call_id: None,
                    });
                }
                ResponseInputItem::FunctionCallOutput {
                    call_id, output, ..
                } => {
                    if !issued_calls.contains(call_id) {
                        return Err(RequestError::BadRequest(format!(
                            "unknown function call_id '{call_id}': no matching function_call in history"
                        )));
                    }
                    // 結果を tool role へ変換。tool_call_id で call と照合。
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: output_to_text(output),
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id.clone()),
                    });
                }
                ResponseInputItem::CustomToolCallOutput {
                    call_id, output, ..
                } => {
                    if !issued_calls.contains(call_id) {
                        return Err(RequestError::BadRequest(format!(
                            "unknown custom tool call_id '{call_id}': no matching call in history"
                        )));
                    }
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: output_to_text(output),
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id.clone()),
                    });
                }
                ResponseInputItem::Reasoning {
                    encrypted_content, ..
                } => {
                    // encrypted reasoning は wire profile で非対応 → 400。
                    if encrypted_content.is_some() {
                        return Err(RequestError::BadRequest(
                            "encrypted reasoning is unsupported (not in the tracked wire profile)"
                                .into(),
                        ));
                    }
                    // 平文 summary は履歴として無視（DS4 には送らない）。
                    // reasoning は推論内部であり、モデルへの入力にしない。
                }
                ResponseInputItem::WebSearchCall { action, .. } => {
                    match action {
                        Some(WebSearchAction::Search { .. }) => {
                            // 検索履歴として保持。再検索はしない。
                            messages.push(ChatMessage {
                                role: "assistant".into(),
                                content: "[web search performed]".into(),
                                ..ChatMessage::default()
                            });
                        }
                        Some(WebSearchAction::OpenPage { .. })
                        | Some(WebSearchAction::FindInPage { .. })
                        | Some(WebSearchAction::Other)
                        | None => {
                            return Err(RequestError::BadRequest(
                                "web_search open_page/find_in_page is unsupported in this bridge"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }

        Ok(messages)
    }

    /// user tool と内部検索 tool 名の衝突を拒否する（受入 case: tool name 衝突 → 400）。
    ///
    /// 内部検索 tool 名 `siderostat_web_search` は Bridge が live search を
    /// 置換するために予約している。user が同名の client function を宣言した
    /// 場合、検索 tool と衝突するため 400 で拒否する。
    pub fn check_tool_name_conflict(
        tools: &[super::wire::ResponseTool],
    ) -> Result<(), RequestError> {
        for tool in tools {
            if let super::wire::ResponseTool::Function(f) = tool {
                if f.name == INTERNAL_SEARCH_TOOL_NAME {
                    return Err(RequestError::BadRequest(format!(
                        "tool name '{INTERNAL_SEARCH_TOOL_NAME}' is reserved for the internal \
                         web search tool; user tools must not collide with it"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// message item の content を text に連結する。image/audio は 400。W02。。
fn message_text(content: &[ContentItem]) -> Result<String, RequestError> {
    let mut text = String::new();
    for part in content {
        match part {
            ContentItem::InputText { text: t } | ContentItem::OutputText { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                return Err(RequestError::BadRequest(
                    "image/audio input is unsupported in this bridge".into(),
                ));
            }
        }
    }
    Ok(text)
}

/// function/custom tool の output を text に変換する。
///
/// output は文字列または structured content items。文字列はそのまま、
/// items は text 要素を連結する。image/audio 要素は無視する（C05 は
/// 結果 URL の fetch を実装しない）。untrusted tool data として扱う。
fn output_to_text(output: &serde_json::Value) -> String {
    match output {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            text
        }
        other => other.to_string(),
    }
}
