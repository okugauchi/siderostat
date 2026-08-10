#![cfg(feature = "test-support")]

mod support;

use anyhow::{Context, Result};
use futures::StreamExt;
use std::{ffi::OsString, time::Duration};
use support::{FakeDs4Process, temporary_path, wait_until_file_exists};

#[tokio::test]
async fn fake_ds4_supports_readiness_streaming_capture_and_sigterm() -> Result<()> {
    let capture_path = temporary_path("argv.json");
    let fake = FakeDs4Process::spawn([
        OsString::from("--startup-delay-ms"),
        OsString::from("120"),
        OsString::from("--chunk-count"),
        OsString::from("3"),
        OsString::from("--mid-stream-close-after-chunks"),
        OsString::from("2"),
        OsString::from("--argv-capture"),
        capture_path.as_os_str().to_owned(),
    ])
    .await?;
    assert!(fake.startup_elapsed >= Duration::from_millis(100));

    let client = reqwest::Client::new();
    let models = client
        .get(format!("http://{}/v1/models", fake.address))
        .send()
        .await?;
    assert!(models.status().is_success());
    assert_eq!(
        models.json::<serde_json::Value>().await?["data"][0]["id"],
        "fake-ds4"
    );

    let response = client
        .post(format!("http://{}/v1/chat/completions", fake.address))
        .body(r#"{"prompt":"must-not-be-logged"}"#)
        .send()
        .await?;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.bytes_stream();
    let first_at = tokio::time::Instant::now();
    let first = stream.next().await.context("first SSE chunk")??;
    let first_elapsed = first_at.elapsed();
    let second_at = tokio::time::Instant::now();
    let second = stream.next().await.context("second SSE chunk")??;
    let second_elapsed = second_at.elapsed();
    assert!(first_elapsed >= Duration::from_millis(70));
    assert!(second_elapsed >= Duration::from_millis(70));
    assert!(String::from_utf8_lossy(&first).contains("token-0"));
    assert!(String::from_utf8_lossy(&second).contains("token-1"));
    assert!(stream.next().await.is_none());

    wait_until_file_exists(&capture_path).await?;
    let capture = tokio::fs::read_to_string(&capture_path).await?;
    assert!(capture.contains("--chunk-count"));
    assert!(!capture.contains("must-not-be-logged"));

    let address = fake.address;
    let status = fake.terminate().await?;
    assert!(status.success());
    assert!(
        client
            .get(format!("http://{address}/v1/models"))
            .send()
            .await
            .is_err()
    );
    tokio::fs::remove_file(capture_path).await?;
    Ok(())
}

#[tokio::test]
async fn fake_ds4_uses_configured_exit_code() -> Result<()> {
    let fake = FakeDs4Process::spawn([
        OsString::from("--exit-after-ms"),
        OsString::from("25"),
        OsString::from("--exit-code"),
        OsString::from("7"),
    ])
    .await?;
    let address = fake.address;
    let status = fake.wait().await?;
    assert_eq!(status.code(), Some(7));
    assert!(
        reqwest::get(format!("http://{address}/v1/models"))
            .await
            .is_err()
    );
    Ok(())
}
