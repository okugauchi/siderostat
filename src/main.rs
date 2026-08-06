use anyhow::Result;
#[tokio::main]
async fn main() -> Result<()> {
    ds4_smart_proxy::cli::run().await
}
