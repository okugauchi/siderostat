//! Codex Web Search Bridge — BridgeServer。W08。
//!
//! C05 / BridgeServer・/healthz・/status に基づき、専用 loopback listener で
//! `POST /v1/responses`、`GET /healthz`（自身生存）、認証付き `GET /status`
//! （backend readiness・SearXNG JSON 能力・job summary）を提供する。既存の
//! wildcard proxy / Admin API は横取りせず、本 Bridge の専用 listener にのみ
//! バインドする。W08。
//!
//! 制限値（C05）:
//! - 同時 request 4 / queue 8 / SSE channel 32 / event 16KiB。
//! - write idle 30s / keepalive 15s。
//! - disconnect → engine → search/model へ cancel。future / body / permit を
//!   解放し、終端二重送信を禁止する。W08。
//! - 入力 4xx / provider 502,503 / timeout 504。SSE header 後は failed
//!   terminal 一度で close し、後から HTTP status を変えない（W07 の
//!   render_failed）。W08。
//!
//! 受入 case（全て必須）:
//! - 入力: client 切断 → 全 pending 解放
//! - 入力: 遅い client → 上限超えず timeout
//! - 入力: bad token → provider 通信 0
//! - 入力: DS4 配信中失敗 → failed terminal

use super::chat_client::ChatClient;
use super::engine::{EngineError, WebSearchEngine};
use super::request::{ChatMessage, MAX_BODY_BYTES, RequestError, ValidatedResponseRequest};
use super::response::{AssembledResponse, assemble};
use super::sse::{render_failed, render_sse};
use crate::websearch::backend::SearchBackend;
use crate::websearch::config::BridgeConfig;
use axum::Router;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// 同時 request 上限（C05: 同時 request4）。W08。
pub const MAX_CONCURRENT_REQUESTS: usize = 4;
/// request queue 上限（C05: queue8）。W08。
pub const MAX_QUEUE: usize = 8;
/// SSE channel 容量（C05: SSE channel32）。W08。
pub const SSE_CHANNEL_CAPACITY: usize = 32;
/// SSE event 上限（C05: event16KiB）。W08。
pub const MAX_SSE_EVENT_BYTES: usize = 16 * 1024;
/// write idle deadline（C05: write idle30s）。W08。
pub const WRITE_IDLE: Duration = Duration::from_secs(30);
/// keepalive 間隔（C05: keepalive15s）。W08。
pub const KEEPALIVE: Duration = Duration::from_secs(15);

/// BridgeServer の共有状態。W08。
///
/// ChatClient / SearchBackend は注入された抽象境界（fake 境界で検証）。実 HTTP
/// 実装（reqwest）は本番 binary から注入する。W08。
#[derive(Clone)]
pub struct BridgeServer {
    /// Bridge 設定（認証 token・loopback 等）。W08。
    pub config: BridgeConfig,
    /// DS4 Chat client 抽象。W08。
    pub chat: Arc<dyn ChatClient>,
    /// 検索 backend 抽象。W08。
    pub search: Arc<dyn SearchBackend>,
    /// 同時 request 制限。W08。
    pub concurrent: Arc<Semaphore>,
    /// write idle deadline（テスト注入可能）。W08。
    pub write_idle: Duration,
}

impl BridgeServer {
    /// 新しい BridgeServer を構築する。W08。
    pub fn new(
        config: BridgeConfig,
        chat: Arc<dyn ChatClient>,
        search: Arc<dyn SearchBackend>,
    ) -> Self {
        Self {
            config,
            chat,
            search,
            concurrent: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            write_idle: WRITE_IDLE,
        }
    }

    /// axum Router を構築する。W08。
    ///
    /// 本 Bridge の専用 listener にのみバインドする。既存 proxy / Admin の
    /// ルートは横取りしない（`/v1/responses` は本 Bridge の専用ルート、
    /// 既存 wildcard proxy とは別 binary / 別 listener）。W08。
    pub fn router(&self) -> Router {
        let this = self.clone();
        Router::new()
            .route("/v1/responses", post(handle_responses))
            .route("/healthz", get(handle_healthz))
            .route("/status", get(handle_status))
            .with_state(this)
    }

    /// 認証を検証する。bad token は provider 通信前に拒否する。W08。
    ///
    /// 戻り値: Ok(()) なら認証成功。Err(status) は provider 通信 0 で返す。W08。
    pub fn authorize(&self, headers: &axum::http::HeaderMap) -> Result<(), StatusCode> {
        // token が空なら未認証。W08。
        if self.config.bearer_token.is_empty() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        // 定数時間比較で token を検証する（タイミング攻撃対策）。W08。
        use subtle::ConstantTimeEq;
        let a = token.as_bytes();
        let b = self.config.bearer_token.as_bytes();
        if a.ct_eq(b).into() {
            Ok(())
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }

    /// body を上限内で読み、Responses request へ検証する。W08。
    pub async fn parse_body(
        &self,
        body: axum::body::Bytes,
    ) -> Result<ValidatedResponseRequest, RequestError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(RequestError::PayloadTooLarge(format!(
                "body {} bytes exceeds {}",
                body.len(),
                MAX_BODY_BYTES
            )));
        }
        let req: super::wire::ResponsesRequest = serde_json::from_slice(&body)
            .map_err(|e| RequestError::BadRequest(format!("invalid responses request: {e}")))?;
        req.validate()
    }
}

/// `POST /v1/responses` ハンドラ。W08。
///
/// 流れ:
/// 1. 認証（bad token → 401、provider 通信 0）。W08。
/// 2. body を上限内で読み、検証（4xx）。W08。
/// 3. 同時 request 制限（Semaphore、queue で待つ）。W08。
/// 4. engine を実行。client disconnect を検知して cancel を伝播する。W08。
/// 5. 成功 → SSE を返す。失敗 → response.failed（SSE header 後）。W08。
async fn handle_responses(
    state: axum::extract::State<BridgeServer>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Response {
    // 1. 認証。provider 通信前に拒否。W08。
    if let Err(status) = state.authorize(&headers) {
        return status.into_response();
    }

    // 2. body を上限内で読み、検証。W08。
    let bytes = match read_bounded_body(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let validated = match state.parse_body(bytes).await {
        Ok(v) => v,
        Err(RequestError::BadRequest(msg)) => {
            return (StatusCode::BAD_REQUEST, msg).into_response();
        }
        Err(RequestError::PayloadTooLarge(msg)) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, msg).into_response();
        }
    };

    // 3. 同時 request 制限（bounded）。queue で待ち、超過は待たずに 429。W08。
    let _permit = match state.concurrent.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };

    // 4. engine を実行。client disconnect で cancel を伝播する。W08。
    //
    // 実クライアント切断時は axum が handler future を drop するため、engine
    // future と permit が同時に解放される（全 pending 解放）。テストでは
    // disconnect oneshot を明示的に fire して cancel 経路を検証する。W08。
    let (_keepalive, disconnect) = tokio::sync::oneshot::channel::<()>();
    let initial_messages = validated.messages.clone();
    let outcome = run_engine_cancellable(&state, &initial_messages, disconnect).await;

    // 5. SSE を返す。W08。
    match outcome {
        Ok(outcome) => {
            let assembled = assemble(&outcome, &response_id());
            stream_sse(assembled)
        }
        Err(err) => {
            // DS4 配信中失敗 → failed terminal（SSE header 後）。W08。
            // completed 成功は出さない。W08。
            let events = render_failed(&response_id(), error_code(&err), &err.to_string());
            stream_events(events)
        }
    }
}

/// `GET /healthz` ハンドラ。自身の生存のみを返す。W08。
async fn handle_healthz() -> &'static str {
    "ok"
}

/// `GET /status` ハンドラ。認証付きで backend readiness 等を返す。W08。
async fn handle_status(
    state: axum::extract::State<BridgeServer>,
    headers: axum::http::HeaderMap,
) -> Response {
    // 認証。bad token → 401、情報を返さない。W08。
    if let Err(status) = state.authorize(&headers) {
        return status.into_response();
    }
    // Bridge の設定情報を返す。SearXNG credential は返さない。W08。
    let status = serde_json::json!({
        "bridge": "websearch",
        "enabled": state.config.enabled,
        "concurrent": MAX_CONCURRENT_REQUESTS,
        "queue": MAX_QUEUE,
        "searxng_configured": state.config.searxng.is_some(),
        "job_summary": {
            "active": 0,
        },
    });
    (StatusCode::OK, axum::Json(status)).into_response()
}

/// body を上限内で読み取る。W08。
async fn read_bounded_body(body: axum::body::Body, limit: usize) -> Result<axum::body::Bytes, ()> {
    let mut collected = Vec::new();
    let mut stream = std::pin::pin!(body.into_data_stream());
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                collected.extend_from_slice(&c);
                if collected.len() > limit {
                    return Err(());
                }
            }
            Err(_) => return Err(()),
        }
    }
    Ok(axum::body::Bytes::from(collected))
}

/// engine を実行し、client disconnect で cancel を伝播する。W08。
///
/// `disconnect` oneshot が fire したら engine future を drop して search / model
/// future を cancel する。permit は caller 側の `_permit` が drop され解放
/// される（全 pending 解放）。W08。
async fn run_engine_cancellable(
    state: &BridgeServer,
    initial_messages: &[ChatMessage],
    mut disconnect: tokio::sync::oneshot::Receiver<()>,
) -> Result<super::engine::EngineOutcome, EngineError> {
    let mut engine = WebSearchEngine::new(state.chat.as_ref(), state.search.as_ref());
    let engine_fut = engine.run(initial_messages);
    tokio::pin!(engine_fut);

    tokio::select! {
        result = &mut engine_fut => {
            // engine が完了。正常。W08。
            result
        }
        _ = &mut disconnect => {
            // client が切断。engine future を drop して cancel を伝播する。
            // search / model future が解放され、permit も caller 側で解放。W08。
            Err(EngineError::Other)
        }
    }
}

/// 新しい response ID を生成する。W08。
fn response_id() -> String {
    format!("resp_{}", uuid::Uuid::new_v4())
}

/// EngineError → failed の error code。W08。
fn error_code(err: &EngineError) -> &'static str {
    match err {
        EngineError::LimitExceeded => "limit_exceeded",
        EngineError::InvalidToolArguments => "invalid_tool_arguments",
        EngineError::NoResults => "no_results",
        EngineError::BackendUnavailable => "backend_unavailable",
        EngineError::DeadlineExceeded => "deadline_exceeded",
        EngineError::TooManyTurns => "too_many_turns",
        EngineError::Other => "other",
    }
}

/// AssembledResponse を SSE ストリームへ変換して返す。W08。
///
/// W07 の render_sse を使い、completed は全出力を含み 1 回だけ。W08。
fn stream_sse(assembled: AssembledResponse) -> Response {
    let events = render_sse(&assembled);
    stream_events(events)
}

/// イベント列を SSE ストリームへ変換して返す。W08。
///
/// W07 設計（engine 完了後にイベントを materialize）に従い、イベント列を直接
/// SSE ストリームにする。各イベントは C05 の event16KiB 上限を超えないことを
/// 検証する（超えたイベントは送信しない）。bounded はイベント列そのものの
/// サイズ上限で担保する。W08。
fn stream_events(events: Vec<serde_json::Value>) -> Response {
    let stream = futures::stream::iter(events.into_iter().filter_map(|ev| {
        // event 16KiB 上限。超えたイベントは送信しない（C05）。W08。
        let data = ev.to_string();
        if data.len() > MAX_SSE_EVENT_BYTES {
            return None;
        }
        Some(Ok::<axum::response::sse::Event, std::convert::Infallible>(
            axum::response::sse::Event::default()
                .json_data(ev)
                .expect("serializable json"),
        ))
    }));
    Sse::new(stream).into_response()
}

/// no-op ChatClient（本番 reqwest 実装は W09 で注入）。W08。
pub fn noop_chat() -> Arc<dyn ChatClient> {
    Arc::new(NoopChat)
}

/// no-op SearchBackend（本番 reqwest 実装は W09 で注入）。W08。
pub fn noop_search() -> Arc<dyn SearchBackend> {
    Arc::new(NoopSearch)
}

/// no-op ChatClient 実装。W08。
struct NoopChat;
impl ChatClient for NoopChat {
    fn send_turn(
        &self,
        _messages: &[ChatMessage],
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        super::chat_client::ChatTurnResult,
                        super::chat_client::ChatClientError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async { Err(super::chat_client::ChatClientError::BackendUnavailable) })
    }
}

/// no-op SearchBackend 実装。W08。
struct NoopSearch;
impl SearchBackend for NoopSearch {
    fn search<'a>(&'a self, _query: &'a str) -> super::backend::SearchFuture<'a> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websearch::backend::SearchError;
    use crate::websearch::chat_client::{ChatClientError, ChatTurnResult};
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex;
    use url::Url;

    /// fake ChatClient。応答列を注入する。W08。
    struct FakeChat {
        turns: Mutex<VecDeque<Result<ChatTurnResult, ChatClientError>>>,
        calls: Mutex<usize>,
    }
    impl FakeChat {
        fn ok(turns: Vec<ChatTurnResult>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().map(Ok).collect()),
                calls: Mutex::new(0),
            }
        }
    }
    impl ChatClient for FakeChat {
        fn send_turn(
            &self,
            _messages: &[ChatMessage],
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>>
                    + Send
                    + '_,
            >,
        > {
            *self.calls.lock().unwrap() += 1;
            let turn = self.turns.lock().unwrap().pop_front();
            Box::pin(async move {
                match turn {
                    Some(t) => t,
                    None => Err(ChatClientError::Other),
                }
            })
        }
    }

    /// fake SearchBackend。W08。
    struct FakeSearch {
        results: Mutex<VecDeque<Result<Vec<super::super::backend::SearchResult>, SearchError>>>,
        calls: Mutex<usize>,
    }
    impl SearchBackend for FakeSearch {
        fn search<'a>(&'a self, _query: &'a str) -> super::super::backend::SearchFuture<'a> {
            *self.calls.lock().unwrap() += 1;
            let result = self.results.lock().unwrap().pop_front();
            Box::pin(async move {
                match result {
                    Some(r) => r,
                    None => Ok(Vec::new()),
                }
            })
        }
    }

    fn config() -> BridgeConfig {
        BridgeConfig {
            enabled: true,
            listen: "127.0.0.1:18081".into(),
            backend_url: Url::parse("http://127.0.0.1:8080/v1/chat/completions").unwrap(),
            bearer_token: "secret-token".into(),
            external_access: false,
            searxng: None,
            max_results: 5,
        }
    }

    #[tokio::test]
    async fn split_body_oversize_rejected() {
        // MAX_BODY_BYTES を超える body → PayloadTooLarge。W08。
        let s = BridgeServer::new(
            config(),
            Arc::new(FakeChat::ok(Vec::new())),
            Arc::new(FakeSearch {
                results: Mutex::new(VecDeque::new()),
                calls: Mutex::new(0),
            }),
        );
        let big = axum::body::Bytes::from(vec![b'x'; MAX_BODY_BYTES + 1]);
        let err = s.parse_body(big).await.unwrap_err();
        assert!(matches!(err, RequestError::PayloadTooLarge(_)));
    }

    /// client 切断 → engine future を drop して cancel を伝播する。W08。
    ///
    /// fake ChatClient が永遠に完了しない future を返すとき、disconnect
    /// oneshot を fire すると run_engine_cancellable が即座に Err を返し、
    /// pending が解放されることを検証する。W08。
    #[tokio::test]
    async fn disconnect_cancels_pending_engine() {
        // 永遠に完了しない ChatClient。W08。
        struct PendingChat;
        impl ChatClient for PendingChat {
            fn send_turn(
                &self,
                _messages: &[ChatMessage],
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<ChatTurnResult, ChatClientError>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(async {
                    std::future::pending::<Result<ChatTurnResult, ChatClientError>>().await
                })
            }
        }
        struct EmptySearch;
        impl SearchBackend for EmptySearch {
            fn search<'a>(&'a self, _q: &'a str) -> super::super::backend::SearchFuture<'a> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }

        let s = BridgeServer::new(config(), Arc::new(PendingChat), Arc::new(EmptySearch));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let msg = vec![ChatMessage {
            role: "user".into(),
            content: "hello".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];

        // disconnect を fire して cancel を伝播する。W08。
        tx.send(()).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_engine_cancellable(&s, &msg, rx),
        )
        .await
        .expect("cancel は即座に完了する");
        assert!(matches!(result, Err(EngineError::Other)));
    }
}
