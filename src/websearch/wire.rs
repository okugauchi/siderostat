//! Codex Web Search Bridge — Responses API request の wire 形式。
//!
//! C05（Responses / Web Search）に基づき、`POST /v1/responses` の request
//! body を typed に受信する。未知の意味ある field を黙って捨てないため、
//! トップレベルは `deny_unknown_fields` で未知 field を拒否する（400）。
//!
//! 契約: CONTRACTS.md C05 / ValidatedResponseRequest。
//! 本 module は wire（受信 JSON）を定義し、検証・Chat 変換は
//! [`crate::websearch::request`] が行う。

use serde::Deserialize;
use serde_json::Value;

/// `POST /v1/responses` の request body。
///
/// C05 入力表に対応する。`deny_unknown_fields` により、未知の意味ある
/// field を黙って捨てず 400 にする（事後条件）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesRequest {
    /// 対象 model。DS4 Chat へ明示変換する。
    pub model: String,
    /// input（文字列または message 配列）。Chat messages へ順序保持で変換。
    pub input: ResponseInput,
    /// instructions（system 指示）。先頭に system message として変換。
    #[serde(default)]
    pub instructions: Option<String>,
    /// stream 指定。DS4 Chat の stream へ明示変換。
    #[serde(default)]
    pub stream: Option<bool>,
    /// max_output_tokens。DS4 Chat へ明示変換。
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// tools。web_search（hosted）と function（client）を含む。
    #[serde(default)]
    pub tools: Option<Vec<ResponseTool>>,
    /// tool_choice。auto/none/required または特定 tool。
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    /// text format 制御。MVP では非対応形式を 400 にする（request.rs）。
    #[serde(default)]
    pub text: Option<TextControls>,
    /// store=true は MVP stateless 非対応 → 400（request.rs）。
    #[serde(default)]
    pub store: Option<bool>,
    /// previous_response_id は MVP stateless 非対応 → 400（request.rs）。
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// reasoning は MVP 非対応 → 400（request.rs）。
    #[serde(default)]
    pub reasoning: Option<Value>,
    /// include 指定。MVP では非対応 → 400（request.rs）。
    #[serde(default)]
    pub include: Option<Vec<String>>,
    /// parallel_tool_calls。DS4 Chat へ明示変換。
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
}

/// Responses API の `input` フィールド。文字列または item 配列。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    /// 文字列入力（user message として変換）。
    String(String),
    /// item 配列（順序保持で変換）。
    Items(Vec<ResponseInputItem>),
}

/// Responses API の input item。
///
/// `type` で判別（snake_case）。未知の item type は serde の unknown variant
/// として拒否し、黙って捨てない。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputItem {
    /// message item。role と content を持つ。順序保持で Chat messages へ。
    Message {
        role: String,
        content: Vec<ContentItem>,
    },
    /// assistant の function call（client tool 呼び出し）。Bridge は実行しない。
    /// call_id を保持し、次ターンの output と照合する。W03。
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        name: String,
        /// arguments は JSON 文字列。parse/validate する。
        arguments: String,
        call_id: String,
    },
    /// assistant の custom tool call。Bridge は実行しない。W03。
    CustomToolCall {
        #[serde(default)]
        id: Option<String>,
        name: String,
        /// input は JSON 文字列。
        input: String,
        call_id: String,
    },
    /// function_call_output（client function の結果）。W03 で往復変換。
    /// call_id は先行 function_call と照合する。
    FunctionCallOutput {
        #[serde(default)]
        name: Option<String>,
        call_id: String,
        output: Value,
    },
    /// custom tool call output。W03 で往復変換。
    CustomToolCallOutput {
        #[serde(default)]
        name: Option<String>,
        call_id: String,
        output: Value,
    },
    /// reasoning 履歴。encrypted_content は MVP 非対応 → 400。W03。
    Reasoning {
        #[serde(default)]
        summary: Vec<Value>,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    /// web_search_call 履歴。検索自体は再実行しない（W06）。
    WebSearchCall {
        id: String,
        status: Option<String>,
        #[serde(default)]
        action: Option<WebSearchAction>,
    },
}

/// message item の content 要素。
///
/// MVP は text のみ対応。image/audio は明示 unsupported（400）にする。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    /// 入力 text。
    InputText { text: String },
    /// 出力 text（assistant 履歴）。
    OutputText { text: String },
    /// 入力 image。MVP 非対応 → 400。
    InputImage { image_url: String },
    /// 入力 audio。MVP 非対応 → 400。
    InputAudio { audio_url: String },
}

/// Responses API の tool 宣言。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseTool {
    /// hosted web_search tool（A03 で wire 固定）。
    #[serde(rename = "web_search")]
    WebSearch(WebSearchTool),
    /// client function tool。Bridge は実行しない（W03 で往復変換）。
    #[serde(rename = "function")]
    Function(FunctionTool),
}

/// web_search hosted tool（A03 wire-profile.json の request_wire.web_search_tool）。
///
/// mode mapping（A03）:
/// - disabled: tool を送らない
/// - cached: external=false, indexed=null（既定）
/// - indexed: external=true, indexed=true
/// - live: external=true, indexed=null
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchTool {
    /// 外部 Web アクセス（live）。
    #[serde(default)]
    pub external_web_access: bool,
    /// indexed Web アクセス。true は unsupported → 400。
    #[serde(default)]
    pub indexed_web_access: Option<bool>,
    /// domain filter。MVP では unsupported → 400。
    #[serde(default)]
    pub filters: Option<WebSearchFilters>,
    /// user location。MVP では unsupported → 400。
    #[serde(default)]
    pub user_location: Option<Value>,
    /// search context size（low/high）。
    #[serde(default)]
    pub search_context_size: Option<String>,
    /// search content types（text/image）。image は unsupported → 400。
    #[serde(default)]
    pub search_content_types: Option<Vec<String>>,
}

/// web_search の domain filter。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchFilters {
    pub allowed_domains: Option<Vec<String>>,
    pub excluded_domains: Option<Vec<String>>,
}

/// client function tool。Bridge は実行しない（W03）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// tool_choice。文字列（auto/none/required）または特定 tool 指定。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    /// "auto" / "none" / "required"。
    String(String),
    /// 特定 tool 指定 { type, name }。
    Specific {
        #[serde(rename = "type")]
        kind: String,
        name: String,
    },
}

/// text format 制御。MVP では既定以外を 400 にする。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextControls {
    #[serde(default)]
    pub format: Option<TextFormat>,
}

/// text format。MVP では text（既定）以外を unsupported。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    Text,
    JsonObject,
    JsonSchema { schema: Value },
}

/// web_search_call の action（履歴）。search のみ保持、他は unsupported。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    /// search。履歴として保持する（再検索しない）。
    Search {
        query: Option<String>,
        #[serde(default)]
        queries: Option<Vec<String>>,
    },
    /// open_page。MVP unsupported → 400。
    OpenPage { url: Option<String> },
    /// find_in_page。MVP unsupported → 400。
    FindInPage {
        url: Option<String>,
        pattern: Option<String>,
    },
    /// 未知 variant。serde(other) 相当は明示拒否。
    #[serde(other)]
    Other,
}
