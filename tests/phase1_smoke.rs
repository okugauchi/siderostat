#![cfg(feature = "test-support")]

mod support;

use anyhow::{Context, Result};
use futures::StreamExt;
use siderostat::{app, config::ModeAwareConfig};
use std::{ffi::OsString, time::Duration};
use support::FakeDs4Process;

#[tokio::test]
async fn solo_standalone_streams_through_schema_v2_runtime() -> Result<()> {
    let fake =
        FakeDs4Process::spawn([OsString::from("--chunk-count"), OsString::from("2")]).await?;
    let mut config = ModeAwareConfig::parse(include_str!("../siderostat.example.toml"))?;
    config.cluster.enabled = false;
    config.ds4.http_host = fake.address.ip();
    config.ds4.http_port = fake.address.port();
    let state = app::AppState::from_config(config)?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let proxy_address = listener.local_addr()?;
    let router = app::public_router(state);
    let proxy_task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let started = tokio::time::Instant::now();
    let response = reqwest::Client::new()
        .post(format!(
            "http://{proxy_address}/v1/chat/completions?smoke=1"
        ))
        .header("authorization", "Bearer smoke-test")
        .body(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await?;
    assert!(response.status().is_success());
    assert_eq!(response.headers()["content-type"], "text/event-stream");

    let mut body = response.bytes_stream();
    let first = body.next().await.context("first proxied SSE frame")??;
    let first_elapsed = started.elapsed();
    let second_started = tokio::time::Instant::now();
    let second = body.next().await.context("second proxied SSE frame")??;
    let second_elapsed = second_started.elapsed();

    assert!(String::from_utf8_lossy(&first).contains("token-0"));
    assert!(String::from_utf8_lossy(&second).contains("token-1"));
    assert!(first_elapsed < Duration::from_millis(250));
    assert!(second_elapsed >= Duration::from_millis(70));

    proxy_task.abort();
    assert!(fake.terminate().await?.success());
    Ok(())
}
