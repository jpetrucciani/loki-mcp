use anyhow::Result;
use clap::Parser;
use loki_mcp::{config, server};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = config::Cli::parse();
    let config = config::load(&cli)?;

    server::run(config).await
}
