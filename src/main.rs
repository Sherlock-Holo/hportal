#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hportal::run().await
}
