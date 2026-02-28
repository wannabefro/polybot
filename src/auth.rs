use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use polymarket_client_sdk::auth::{state::Authenticated, LocalSigner, Normal, Signer as _};
use polymarket_client_sdk::clob::{Client, Config as SdkConfig};
use tracing::info;

use crate::config::Config;

/// Authenticated CLOB client (thread-safe handle).
pub type AuthClient = Arc<Client<Authenticated<Normal>>>;

/// Build an authenticated SDK client from a private key.
///
/// Flow: hex private key → L1 LocalSigner → SDK derives/creates L2 API creds.
pub async fn init(config: &Config) -> Result<AuthClient> {
    info!("auth: creating L1 signer");

    let chain_id = config.chain_id;
    let signer = LocalSigner::from_str(&config.private_key)?
        .with_chain_id(Some(chain_id));

    info!(address = %signer.address(), "auth: L1 signer ready");

    let sdk_config = SdkConfig::builder()
        .use_server_time(true)
        .heartbeat_interval(config.heartbeat_interval)
        .build();

    let client = Client::new(&config.clob_host, sdk_config)?
        .authentication_builder(&signer)
        .authenticate()
        .await?;

    info!("auth: authenticated — L2 credentials active");
    Ok(Arc::new(client))
}
