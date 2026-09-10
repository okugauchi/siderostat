//! Codex Web Search Bridge — Responses request の検証と通常会話変換。
//!
//! C05 に基づき、wire（[`crate::websearch::wire`]）を検証して
//! [`ValidatedResponseRequest`] へ変換する。input/instructions を Chat
//! messages へ順序保持で変換し、unsupported を backend 呼び出し前に 400 で
//! 返す。oversize body は 413 で返す。
//!
//! 契約: CONTRACTS.md C05 / ValidatedResponseRequest。
//!
//! 受入 case（全て必須）:
//! - 入力: system/developer/user → 順序保持
//! - 入力: cached/open_page/filter → 400
//! - 入力: oversize chunked body → 413
//! - 入力: 検索なし → 検索通信0

use super::wire::{ResponseInput, ResponseTool, ResponsesRequest, ToolChoice};

/// 受入 body の上限（C05: body2MiB）。
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// input（input フィールド文字列）の上限（C05: input1MiB）。
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// request 検証エラー。HTTP status へ対応する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// 400 Bad Request。unsupported / 不正 field。
    BadRequest(String),
    /// 413 Payload Too Large。oversize body。
    PayloadTooLarge(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::BadRequest(msg) => write!(f, "bad request: {msg}"),
            RequestError::PayloadTooLarge(msg) => write!(f, "payload too large: {msg}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Chat message（DS4 Chat Completions へ送る形式）。
///
/// - `role`: system / developer / user / assistant / tool。
/// - `content`: テキスト内容。
/// - `tool_calls`: assistant の tool call 一覧（順序保持）。client tool の
///   call_id を保持し、次ターンの output と照合する。W03。
/// - `tool_call_id`: role=tool の場合の対応 call_id。W03。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// Chat tool call（Chat Completions の tool_calls 要素）。W03。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChatToolCall {
    /// 対応 call_id（Responses function_call の call_id と一致）。W03。
    pub id: String,
    /// tool 名。
    pub name: String,
    /// arguments（JSON 文字列）。DS4 から返る形式。
    pub arguments: String,
}

/// 内部検索 tool の予約名（C05 のネストした function 形式）。W03。
///
/// user tool と予約名の衝突は拒否する（受入 case: tool name 衝突 → 400）。
pub const INTERNAL_SEARCH_TOOL_NAME: &str = "siderostat_web_search";

/// 検索要求の検証結果。
///
/// - `live` は live web_search（external=true, indexed=null）が user opt-in
///   付きで要求されたことを示す。
/// - `required` は tool_choice が required / 特定 web_search を指すことを示す。
///   検索なし最終回答を成功にしないため、この場合は検索を必ず実行する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSearch {
    pub live: bool,
    pub required: bool,
    /// 内部検索 tool へ渡す query 上限（C05: query1024 scalar）。
    pub query_max_chars: usize,
}

/// 検証済み Responses request。Chat messages へ変換済み。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedResponseRequest {
    pub model: String,
    /// 順序保持で変換した Chat messages。
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub max_output_tokens: Option<u32>,
    pub parallel_tool_calls: Option<bool>,
    /// 検索要求。`None` なら検索通信は起きない。
    pub search: Option<ValidatedSearch>,
}

/// oversize body を検査する（受入 case: oversize chunked body → 413）。
pub fn check_body_size(body_len: usize) -> Result<(), RequestError> {
    if body_len > MAX_BODY_BYTES {
        return Err(RequestError::PayloadTooLarge(format!(
            "request body exceeds the {MAX_BODY_BYTES}-byte limit"
        )));
    }
    Ok(())
}

/// input 文字列の size を検査する（C05: input1MiB）。
fn check_input_size(input: &str) -> Result<(), RequestError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(RequestError::PayloadTooLarge(format!(
            "input exceeds the {MAX_INPUT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

/// tool_choice が検索を必須化するかを判定する。
///
/// - "required" → 検索必須。
/// - 特定 tool 指定（{type: "web_search", ...}）→ 検索必須。
/// - "auto"/"none" → 必須ではない（model 判断 / 検索しない）。
fn search_required(choice: &Option<ToolChoice>) -> bool {
    match choice {
        Some(ToolChoice::String(s)) => s == "required",
        Some(ToolChoice::Specific { kind, .. }) => kind == "web_search",
        None => false,
    }
}

/// tool_choice が検索を明示的に禁止（none）しているかを判定する。
///
/// live web_search tool があっても tool_choice=none なら検索しない。
fn search_forbidden(choice: &Option<ToolChoice>) -> bool {
    matches!(choice, Some(ToolChoice::String(s)) if s == "none")
}

/// web_search tool を検証する。
///
/// A03 wire-profile: cached（external=false, indexed=null）は受信可能（Codex
/// 既定）。indexed（indexed=true）・filters・user_location・image search は
/// MVP で unsupported → 400。
fn validate_web_search(tool: &super::wire::WebSearchTool) -> Result<bool, RequestError> {
    // indexed（indexed=true）→ 明示 unsupported。
    if tool.indexed_web_access == Some(true) {
        return Err(RequestError::BadRequest(
            "web_search indexed access is unsupported in this bridge".into(),
        ));
    }
    // filters → 明示 unsupported（受信のみ、実行しない）。
    if let Some(filters) = &tool.filters {
        if filters
            .allowed_domains
            .as_deref()
            .is_some_and(|d| !d.is_empty())
            || filters
                .excluded_domains
                .as_deref()
                .is_some_and(|d| !d.is_empty())
        {
            return Err(RequestError::BadRequest(
                "web_search filters are unsupported in this bridge".into(),
            ));
        }
    }
    // user_location → 明示 unsupported。
    if tool.user_location.is_some() {
        return Err(RequestError::BadRequest(
            "web_search user_location is unsupported in this bridge".into(),
        ));
    }
    // image search → 明示 unsupported。
    if let Some(types) = &tool.search_content_types {
        if types.iter().any(|t| t == "image") {
            return Err(RequestError::BadRequest(
                "web_search image search is unsupported in this bridge".into(),
            ));
        }
    }
    // live = external=true かつ indexed が false/None。
    let live = tool.external_web_access && tool.indexed_web_access != Some(true);
    Ok(live)
}

impl ResponsesRequest {
    /// 検証して [`ValidatedResponseRequest`] へ変換する。
    pub fn validate(&self) -> Result<ValidatedResponseRequest, RequestError> {
        // MVP stateless 非対応（C05）。
        if self.store == Some(true) {
            return Err(RequestError::BadRequest(
                "store=true is unsupported (stateless bridge)".into(),
            ));
        }
        if self.previous_response_id.is_some() {
            return Err(RequestError::BadRequest(
                "previous_response_id is unsupported (stateless bridge)".into(),
            ));
        }
        if self.reasoning.is_some() {
            return Err(RequestError::BadRequest(
                "reasoning is unsupported in this bridge".into(),
            ));
        }
        if self.include.as_deref().is_some_and(|i| !i.is_empty()) {
            return Err(RequestError::BadRequest(
                "include is unsupported in this bridge".into(),
            ));
        }
        // text format: text（既定）のみ。json_object/json_schema は unsupported。
        if let Some(text) = &self.text {
            match &text.format {
                None => {}
                Some(super::wire::TextFormat::Text) => {}
                Some(_) => {
                    return Err(RequestError::BadRequest(
                        "text format other than plain text is unsupported in this bridge".into(),
                    ));
                }
            }
        }

        // user tool と内部検索 tool 名の衝突を拒否（受入 case: tool name 衝突 → 400）。W03。
        if let Some(tools) = &self.tools {
            super::history::HistoryAdapter::check_tool_name_conflict(tools)?;
        }

        // 検索要求を判定する。
        let required = search_required(&self.tool_choice);
        let forbidden = search_forbidden(&self.tool_choice);
        let mut live = false;
        let mut has_web_search_tool = false;
        if let Some(tools) = &self.tools {
            for tool in tools {
                match tool {
                    ResponseTool::WebSearch(w) => {
                        has_web_search_tool = true;
                        live = validate_web_search(w)? || live;
                    }
                    ResponseTool::Function(_) => {
                        // client function は Bridge が実行しない。W03 で往復変換。
                        // W02 では通常会話として保持する（実行しない）。
                    }
                }
            }
        }

        // tool_choice が検索を必須化し、かつ live 検索 tool が無い場合、
        // cached（external=false）検索を実行できないため 400 で返す。
        // （受入 case: cached → 400。cached 単独の tool 宣言は Codex 既定
        //  として受信可能だが、必須検索は実行できない。）
        let search = if forbidden {
            // tool_choice=none → 検索しない。検索通信は起きない。
            None
        } else if required && has_web_search_tool && !live {
            return Err(RequestError::BadRequest(
                "required web search is unsupported without a live web_search tool \
                 (cached-only search is not executed by this bridge)"
                    .into(),
            ));
        } else if live {
            Some(ValidatedSearch {
                live: true,
                required,
                query_max_chars: 1024,
            })
        } else if required {
            // required だが web_search tool が無い場合（function tool のみ等）
            // は、検索は要求されていない。通常会話として扱う。
            None
        } else {
            // live でも required でも無い → 検索なし。検索通信は起きない。
            None
        };

        // input/instructions を Chat messages へ順序保持で変換する。
        let messages = self.to_chat_messages()?;

        Ok(ValidatedResponseRequest {
            model: self.model.clone(),
            messages,
            stream: self.stream.unwrap_or(false),
            max_output_tokens: self.max_output_tokens,
            parallel_tool_calls: self.parallel_tool_calls,
            search,
        })
    }

    /// input と instructions を Chat messages へ順序保持で変換する。W03。
    ///
    /// - instructions → 先頭に system message。
    /// - input string → 単一 user message。
    /// - input items → HistoryAdapter で順序保持・call_id 照合・round-trip
    ///   変換（function call ↔ tool_calls、output ↔ tool role）。W03。。
    fn to_chat_messages(&self) -> Result<Vec<ChatMessage>, RequestError> {
        let mut messages = Vec::new();

        if let Some(instructions) = &self.instructions {
            if !instructions.trim().is_empty() {
                messages.push(ChatMessage {
                    role: "system".into(),
                    content: instructions.clone(),
                    ..ChatMessage::default()
                });
            }
        }

        match &self.input {
            ResponseInput::String(text) => {
                check_input_size(text)?;
                messages.push(ChatMessage {
                    role: "user".into(),
                    content: text.clone(),
                    ..ChatMessage::default()
                });
            }
            ResponseInput::Items(items) => {
                let converted = super::history::HistoryAdapter::to_chat_messages(items)?;
                messages.extend(converted);
            }
        }

        Ok(messages)
    }
}
