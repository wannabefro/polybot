use anyhow::Result;
use tracing::info;

/// Placeholder: L1 signer → derive/create L2 credentials → cache
pub async fn init() -> Result<()> {
    info!("auth: initializing credentials");
    // TODO: implement L1 signer -> L2 credential derivation
    Ok(())
}
