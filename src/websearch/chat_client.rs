//! Codex Web Search Bridge — DS4 Chat Completions client。W05。
//!
//! C05 に基づき、DS4 Chat Completions の tool_calls 形式を扱う client 抽象と、
//! bounded function loop に必要な型を定義する。実 HTTP は W08（Bridge 起動）で
//! 実装し、本 module は抽象境界と検証ロジックを提供する。
//!
//! 契約: CONTRACTS.md C05 / WebSearchEngine・EngineOutcome。W05。
//!
//! 受入 case（全て必須）:
//! - 入力: 3検索後4回目 → LimitExceeded
//! - 入力: 壊れたarguments → InvalidToolArguments
//! - 入力: 空結果 → NoResults
//! - 入力: DS4 503 → BackendUnavailable
//! - 入力: snippetの命令 → Bridge実行0

use super::engine::search_tool_definition;
use super::request::{ChatMessage, ChatToolCall};
use reqwest::Url;
use serde_json::Value;
use std::time::Duration;

/// DS4 Chat Completions への送信結果。W05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurnResult {
    /// 完了した turn。`tool_calls` が空なら最終回答。W05。
    pub messages: Vec<ChatMessage>,
    /// model が要求した tool call（順序保持）。空なら回答完了。W05。
    pub tool_calls: Vec<ChatToolCallResult>,
    /// 最終回答 text（tool_calls が空の場合）。W05。
    pub text: String,
}

/// DS4 が返す tool call。W05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatToolCallResult {
    /// call_id。次ターン output との照合に使用。W05。
    pub id: String,
    /// tool 名。`siderostat_web_search` は内部検索、それ以外は client tool。W05。
    pub name: String,
    /// arguments（JSON 文字列）。JSON schema で検証する。W05。
    pub arguments: String,
}

/// DS4 Chat Completions client の抽象。W05。/
///
/// 実装は W08（Bridge 起動）で reqwest を使う。テストは fake 境界で応答を
/// 注入する。
pub trait ChatClient: Send + Sync {
    /// 1 turn 分の Chat Completions を送信する。W05。/
    ///
    /// 引数の messages は順序保持。tool_calls（assistant）と tool output
    /// （untrusted tool message）を含む。戻り値に tool_calls があれば
    /// 続けて送信する（bounded loop）。W05。/
    fn send_turn(
        &self,
        messages: &[ChatMessage],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>> + Send + '_>,
    >;

    /// 初回turnで検索を必須化する。既存のfake clientは通常turnへ委譲する。H05。
    fn send_turn_with_search_requirement(
        &self,
        messages: &[ChatMessage],
        require_search: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>> + Send + '_>,
    > {
        let _ = require_search;
        self.send_turn(messages)
    }
}

/// DS4 Chat client エラー。W05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatClientError {
    /// DS4 が 503 を返した（backend ready 失敗）。W05。
    BackendUnavailable,
    /// 通信 timeout。W05。
    Timeout,
    /// その他（接続不可・5xx 等）。W05。
    Other,
}

/// bounded loop の共有 deadline（C05: 総600s）。W05。
pub const TOTAL_DEADLINE: Duration = Duration::from_secs(600);
/// 検索回数上限（C05: search最大3、retry含む）。W05。
pub const MAX_SEARCHES: usize = 3;
/// model turn 上限（C05: model turn6）。W05。
pub const MAX_MODEL_TURNS: usize = 6;
/// 単一検索の timeout（C05: search30s）。W05。
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// 単一 model turn の timeout（C05: model300s）。W05。
pub const MODEL_TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// 本番DS4 Chat Completions client。W09/H05。
///
/// DS4のOpenAI互換chat endpointへ1 turnずつ送信し、tool_callsを
/// [`WebSearchEngine`](super::engine::WebSearchEngine)へ返す。認証情報は持たず、
/// endpointはBridgeConfigから明示的に受け取る。
#[derive(Clone)]
pub struct ReqwestChatClient {
    client: reqwest::Client,
    endpoint: Url,
    model: String,
    max_output_tokens: u32,
}

impl ReqwestChatClient {
    /// DS4 endpointを構築する。redirectは無効にし、model turnのdeadlineを適用する。
    pub fn new(endpoint: Url) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(MODEL_TURN_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            endpoint,
            model: "deepseek-v4-flash".into(),
            max_output_tokens: 1024,
        })
    }

    /// 使用するDS4 model aliasを設定する。
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// 最大出力token数を設定する。
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = max_output_tokens.max(1);
        self
    }

    async fn send_http(
        &self,
        messages: &[ChatMessage],
        require_search: bool,
    ) -> Result<ChatTurnResult, ChatClientError> {
        let body = request_body(
            &self.model,
            messages,
            self.max_output_tokens,
            require_search,
        );
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            // 503 bodyはJSONでない場合もあるため、statusを先に分類する。
            let _ = response.bytes().await;
            return Err(if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                ChatClientError::BackendUnavailable
            } else {
                ChatClientError::Other
            });
        }
        let value: Value = response.json().await.map_err(|_| ChatClientError::Other)?;
        parse_chat_response(value)
    }
}

fn request_body(
    model: &str,
    messages: &[ChatMessage],
    max_output_tokens: u32,
    require_search: bool,
) -> Value {
    let mut request_messages: Vec<Value> = messages.iter().map(chat_message_to_wire).collect();
    // DS4へ検索結果の引用形式を明示する。結果本文はuntrusted dataだが、
    // 回答中の[N] markerはBridge側で実URLへ検証・変換する。
    request_messages.insert(
        0,
        serde_json::json!({
            "role": "system",
            "content": "When using the web search tool, cite factual claims with [1], [2], etc. matching the numbered search results. Do not invent citation numbers.",
        }),
    );
    serde_json::json!({
        "model": model,
        "messages": request_messages,
        "tools": [search_tool_definition()],
        "tool_choice": if require_search { "required" } else { "auto" },
        "stream": false,
        "max_tokens": max_output_tokens,
        "reasoning_effort": "none",
    })
}

impl ChatClient for ReqwestChatClient {
    fn send_turn(
        &self,
        messages: &[ChatMessage],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>> + Send + '_>,
    > {
        let messages = messages.to_vec();
        Box::pin(async move { self.send_http(&messages, false).await })
    }

    fn send_turn_with_search_requirement(
        &self,
        messages: &[ChatMessage],
        require_search: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>> + Send + '_>,
    > {
        let messages = messages.to_vec();
        Box::pin(async move { self.send_http(&messages, require_search).await })
    }
}

fn map_reqwest_error(error: reqwest::Error) -> ChatClientError {
    if error.is_timeout() {
        ChatClientError::Timeout
    } else if error.is_connect() || error.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
    {
        ChatClientError::BackendUnavailable
    } else {
        ChatClientError::Other
    }
}

fn chat_message_to_wire(message: &ChatMessage) -> Value {
    let mut wire = serde_json::json!({
        "role": message.role,
        "content": message.content,
    });
    if !message.tool_calls.is_empty() {
        wire["tool_calls"] = Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    serde_json::json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        wire["tool_call_id"] = Value::String(tool_call_id.clone());
    }
    wire
}

fn parse_chat_response(value: Value) -> Result<ChatTurnResult, ChatClientError> {
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or(ChatClientError::Other)?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or(ChatClientError::Other)?;
            let function = call.get("function").ok_or(ChatClientError::Other)?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or(ChatClientError::Other)?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or(ChatClientError::Other)?;
            tool_calls.push(ChatToolCallResult {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            });
        }
    }
    Ok(ChatTurnResult {
        messages: vec![ChatMessage {
            role: "assistant".into(),
            content: text.clone(),
            tool_calls: tool_calls
                .iter()
                .map(|call| ChatToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
            tool_call_id: None,
        }],
        tool_calls,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_messages_use_openai_tool_call_shape() {
        let message = ChatMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![ChatToolCall {
                id: "call_1".into(),
                name: "siderostat_web_search".into(),
                arguments: r#"{"query":"Rust"}"#.into(),
            }],
            tool_call_id: None,
        };
        let wire = chat_message_to_wire(&message);
        assert_eq!(wire["tool_calls"][0]["type"], "function");
        assert_eq!(
            wire["tool_calls"][0]["function"]["name"],
            "siderostat_web_search"
        );
        assert_eq!(
            wire["tool_calls"][0]["function"]["arguments"],
            r#"{"query":"Rust"}"#
        );
    }

    #[test]
    fn parses_ds4_tool_call_response() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "siderostat_web_search",
                            "arguments": "{\"query\":\"Rust\"}"
                        }
                    }]
                }
            }]
        });
        let turn = parse_chat_response(response).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "siderostat_web_search");
    }

    #[test]
    fn malformed_ds4_response_fails_closed() {
        assert_eq!(
            parse_chat_response(serde_json::json!({"choices": []})),
            Err(ChatClientError::Other)
        );
    }

    #[test]
    fn required_search_is_encoded_as_required_tool_choice() {
        let body = request_body(
            "deepseek-v4-flash",
            &[ChatMessage {
                role: "user".into(),
                content: "search this".into(),
                ..ChatMessage::default()
            }],
            64,
            true,
        );
        assert_eq!(body["tool_choice"], "required");
    }

    #[tokio::test]
    async fn connection_refused_is_backend_unavailable() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:9/v1/chat/completions")
            .send()
            .await
            .expect_err("test port must be closed");
        assert_eq!(
            map_reqwest_error(error),
            ChatClientError::BackendUnavailable
        );
    }
}
