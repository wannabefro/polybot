mod config;
mod geoblock;
mod auth;
mod market;
mod order;
mod risk;
mod strategy;
mod intelligence;
mod ops;

use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "polybot=info".into()),
        )
        .json()
        .init();

    info!("polybot starting");

    // Phase 1: compliance gate
    geoblock::check_or_abort().await?;

    // Phase 2: auth
    let _credentials = auth::init().await?;

    // Phase 3: market discovery + data feeds
    // Phase 4: risk engine + strategies
    // Phase 5: main event loop

    info!("polybot ready — entering event loop (paper mode)");

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}
