//! Codex Web Search Bridge — 独立モジュール。
//!
//! C05（Responses / Web Search）に基づき、Web Search Bridge を同 crate の
//! 独立 module として用意する。本モジュールは Bridge の typed 設定
//! （[`config::BridgeConfig`]）を提供し、検索 backend の抽象
//! （[`search`] は後続 task で追加）を内包する。
//!
//! 契約: CONTRACTS.md C05 / BridgeConfig。
//! 既定 disabled: 設定だけで検索や既存 runtime 停止は起きない。

pub mod config;
pub mod history;
pub mod request;
pub mod wire;

pub use config::BridgeConfig;
pub use history::HistoryAdapter;
pub use request::{ChatMessage, ChatToolCall, RequestError, ValidatedResponseRequest};
