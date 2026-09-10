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

use super::request::ChatMessage;
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
