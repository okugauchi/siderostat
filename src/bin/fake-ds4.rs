use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode, header},
    routing::{any, get},
};
use bytes::Bytes;
use clap::Parser;
use futures::stream;
use serde_json::{Value, json};
use std::{
    convert::Infallible, net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc,
    time::Duration,
};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,

    #[arg(long, default_value_t = 0)]
    startup_delay_ms: u64,

    #[arg(long, default_value_t = 3)]
    chunk_count: usize,

    #[arg(long)]
    mid_stream_close_after_chunks: Option<usize>,

    #[arg(long)]
    exit_after_ms: Option<u64>,

    #[arg(long, default_value_t = 0)]
    exit_code: u8,

    #[arg(long)]
    argv_capture: Option<PathBuf>,

    #[arg(long)]
    emit_dspark_activation: bool,
}

#[derive(Debug)]
struct FakeState {
    chunk_count: usize,
    mid_stream_close_after_chunks: Option<usize>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("fake-ds4 failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u8> {
    let args = Args::parse();
    capture_argv(args.argv_capture.as_ref()).await?;
    tokio::time::sleep(Duration::from_millis(args.startup_delay_ms)).await;

    let state = Arc::new(FakeState {
        chunk_count: args.chunk_count,
        mid_stream_close_after_chunks: args.mid_stream_close_after_chunks,
    });
    let app = Router::new()
        .route("/v1/models", get(models))
        .fallback(any(streaming_response))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    let address = listener.local_addr().context("read listener address")?;
    if args.emit_dspark_activation {
        eprintln!("ds4: DSpark target-hidden capture enabled: layers=3,7,11");
    }
    println!("fake-ds4 listening on {address}");

    let server = axum::serve(listener, app);
    tokio::select! {
        result = server => result.context("serve fake DS4")?,
        () = termination_signal() => {},
        () = exit_timer(args.exit_after_ms) => {},
    }
    Ok(args.exit_code)
}

async fn capture_argv(path: Option<&PathBuf>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let argv = std::env::args_os()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec_pretty(&argv).context("serialize argv")?;
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("write argv capture {}", path.display()))
}

async fn models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{"id": "fake-ds4", "object": "model"}],
    }))
}

async fn streaming_response(State(state): State<Arc<FakeState>>) -> Response<Body> {
    let stream = stream::unfold(0_usize, move |index| {
        let state = state.clone();
        async move {
            if state
                .mid_stream_close_after_chunks
                .is_some_and(|limit| index >= limit)
                || index > state.chunk_count
            {
                return None;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
            let frame = if index == state.chunk_count {
                Bytes::from_static(b"data: [DONE]\n\n")
            } else {
                Bytes::from(format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"token-{index}\"}}}}]}}\n\n"
                ))
            };
            Some((Ok::<_, Infallible>(frame), index + 1))
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("valid fake DS4 response")
}

async fn exit_timer(delay_ms: Option<u64>) {
    match delay_ms {
        Some(delay_ms) => tokio::time::sleep(Duration::from_millis(delay_ms)).await,
        None => std::future::pending().await,
    }
}

#[cfg(unix)]
async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = terminate.recv() => {},
        _ = interrupt.recv() => {},
    }
}

#[cfg(not(unix))]
async fn termination_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
