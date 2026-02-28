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

    let cfg = config::Config::from_env()?;

    // Phase 1: compliance gate
    geoblock::check_or_abort().await?;
    let (_geo_handle, mut geo_rx) = geoblock::spawn_monitor(&cfg);

    // Phase 2: auth
    let _client = auth::init(&cfg).await?;

    // Phase 3: market discovery + data feeds
    // Phase 4: risk engine + strategies
    // Phase 5: main event loop

    info!("polybot ready — entering event loop (paper mode)");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received — shutting down");
        }
        _ = geo_rx.changed() => {
            info!("geoblock triggered — emergency shutdown");
        }
    }
    Ok(())
}
