#![cfg(feature = "test-support")]

mod support;

use anyhow::Result;
use siderostat::canary::{CanaryExecutor, CanaryPolicy, CanaryReason};
use std::{ffi::OsString, time::Duration};
use support::FakeDs4Process;

fn test_policy() -> CanaryPolicy {
    CanaryPolicy {
        deadline: Duration::from_secs(2),
        progress_stall: Duration::from_millis(250),
        low_decode_tps: 5.0,
    }
}

#[tokio::test]
async fn canary_accepts_a_normal_stream() -> Result<()> {
    let fake = FakeDs4Process::spawn([
        OsString::from("--chunk-count"),
        OsString::from("3"),
        OsString::from("--chunk-delay-ms"),
        OsString::from("20"),
    ])
    .await?;
    let executor = CanaryExecutor::new(fake.address, test_policy())?;

    let result = executor.execute().await;

    assert_eq!(result.reason, CanaryReason::Healthy);
    assert_eq!(result.status, "healthy");
    assert_eq!(result.generated_tokens, 3);
    assert_eq!(result.http_status, Some(200));
    assert!(result.ttfb_ms.is_some());
    assert!(result.chunk_tps.is_some_and(|tps| tps >= 5.0));
    assert!(fake.terminate().await?.success());
    Ok(())
}

#[tokio::test]
async fn canary_reports_low_decode_tps() -> Result<()> {
    let fake = FakeDs4Process::spawn([
        OsString::from("--chunk-count"),
        OsString::from("3"),
        OsString::from("--chunk-delay-ms"),
        OsString::from("300"),
    ])
    .await?;
    let executor = CanaryExecutor::new(
        fake.address,
        CanaryPolicy {
            progress_stall: Duration::from_secs(1),
            ..test_policy()
        },
    )?;

    let result = executor.execute().await;

    assert_eq!(result.reason, CanaryReason::LowDecodeTps);
    assert_eq!(result.status, "failed");
    assert!(result.chunk_tps.is_some_and(|tps| tps < 5.0));
    assert!(fake.terminate().await?.success());
    Ok(())
}

#[tokio::test]
async fn canary_reports_first_token_deadline() -> Result<()> {
    let fake = FakeDs4Process::spawn([
        OsString::from("--first-chunk-delay-ms"),
        OsString::from("500"),
    ])
    .await?;
    let executor = CanaryExecutor::new(
        fake.address,
        CanaryPolicy {
            deadline: Duration::from_millis(100),
            ..test_policy()
        },
    )?;

    let result = executor.execute().await;

    assert_eq!(result.reason, CanaryReason::Deadline);
    assert_eq!(result.status, "failed");
    assert!(result.ttfb_ms.is_none());
    assert!(fake.terminate().await?.success());
    Ok(())
}

#[tokio::test]
async fn canary_reports_mid_stream_progress_stall() -> Result<()> {
    let fake = FakeDs4Process::spawn([
        OsString::from("--chunk-count"),
        OsString::from("3"),
        OsString::from("--chunk-delay-ms"),
        OsString::from("20"),
        OsString::from("--stall-after-chunks"),
        OsString::from("1"),
    ])
    .await?;
    let executor = CanaryExecutor::new(fake.address, test_policy())?;

    let result = executor.execute().await;

    assert_eq!(result.reason, CanaryReason::ProgressStall);
    assert_eq!(result.status, "failed");
    assert_eq!(result.generated_tokens, 1);
    assert!(fake.terminate().await?.success());
    Ok(())
}

#[tokio::test]
async fn canary_reports_http_error_without_response_body() -> Result<()> {
    let fake =
        FakeDs4Process::spawn([OsString::from("--http-status"), OsString::from("503")]).await?;
    let executor = CanaryExecutor::new(fake.address, test_policy())?;

    let result = executor.execute().await;

    assert_eq!(result.reason, CanaryReason::HttpError);
    assert_eq!(result.status, "failed");
    assert_eq!(result.http_status, Some(503));
    assert_eq!(result.generated_tokens, 0);
    assert!(fake.terminate().await?.success());
    Ok(())
}
