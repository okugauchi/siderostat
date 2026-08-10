use anyhow::Result;
#[tokio::main]
async fn main() -> Result<()> {
    siderostat::cli::run().await
}
