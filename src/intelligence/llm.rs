use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::intelligence::signal::{RiskSignal, SignalSender};

/// Raw LLM response (strict schema enforced by prompt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub pull: bool,
    pub risk_multiplier: f64,
    pub ttl_ms: u64,
    pub reason_code: String,
}

/// Configuration for the LLM inference loop.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// How often to poll for new intelligence.
    pub poll_interval: Duration,
    /// Maximum time to wait for a single LLM call.
    pub timeout: Duration,
    /// API endpoint for the LLM service.
    pub endpoint: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            timeout: Duration::from_millis(1500),
            endpoint: None,
        }
    }
}

/// Validate an LLM response before converting to a RiskSignal.
pub fn validate_response(resp: &LlmResponse) -> Result<RiskSignal> {
    if resp.risk_multiplier < 0.0 || resp.risk_multiplier > 1.0 {
        anyhow::bail!(
            "risk_multiplier out of range: {} (must be 0.0..1.0)",
            resp.risk_multiplier
        );
    }
    if resp.ttl_ms == 0 {
        anyhow::bail!("ttl_ms must be > 0");
    }
    if resp.reason_code.is_empty() {
        anyhow::bail!("reason_code must not be empty");
    }
    if resp.reason_code.len() > 64 {
        anyhow::bail!("reason_code too long (max 64 chars)");
    }

    Ok(RiskSignal {
        pull: resp.pull,
        risk_multiplier: resp.risk_multiplier,
        ttl_ms: resp.ttl_ms,
        reason_code: resp.reason_code.clone(),
        created_at: Some(Instant::now()),
    })
}

/// Spawn the LLM intelligence loop.
///
/// Periodically calls the LLM endpoint, validates the response, and
/// publishes RiskSignals through the signal bridge.
pub fn spawn(
    llm_config: LlmConfig,
    signal_tx: SignalSender,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = time::interval(llm_config.poll_interval);

        loop {
            ticker.tick().await;

            let endpoint = match &llm_config.endpoint {
                Some(ep) => ep.clone(),
                None => {
                    debug!("llm: no endpoint configured, skipping");
                    continue;
                }
            };

            match tokio::time::timeout(
                llm_config.timeout,
                call_llm(&endpoint),
            )
            .await
            {
                Ok(Ok(resp)) => match validate_response(&resp) {
                    Ok(signal) => {
                        info!(
                            pull = signal.pull,
                            multiplier = signal.risk_multiplier,
                            reason = %signal.reason_code,
                            "llm: new signal"
                        );
                        let _ = signal_tx.send(Arc::new(signal));
                    }
                    Err(e) => {
                        warn!("llm: invalid response: {e}");
                    }
                },
                Ok(Err(e)) => {
                    warn!("llm: call failed: {e}");
                }
                Err(_) => {
                    warn!("llm: call timed out ({}ms)", llm_config.timeout.as_millis());
                }
            }
        }
    })
}

/// Call the LLM endpoint and parse the response.
async fn call_llm(endpoint: &str) -> Result<LlmResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .get(endpoint)
        .timeout(Duration::from_millis(1500))
        .send()
        .await?
        .json::<LlmResponse>()
        .await?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_response_converts() {
        let resp = LlmResponse {
            pull: false,
            risk_multiplier: 0.7,
            ttl_ms: 30000,
            reason_code: "market_stable".into(),
        };
        let signal = validate_response(&resp).unwrap();
        assert!(!signal.pull);
        assert_eq!(signal.risk_multiplier, 0.7);
        assert_eq!(signal.ttl_ms, 30000);
        assert_eq!(signal.reason_code, "market_stable");
        assert!(signal.created_at.is_some());
    }

    #[test]
    fn pull_response_converts() {
        let resp = LlmResponse {
            pull: true,
            risk_multiplier: 0.0,
            ttl_ms: 5000,
            reason_code: "breaking_news".into(),
        };
        let signal = validate_response(&resp).unwrap();
        assert!(signal.pull);
        assert_eq!(signal.effective_multiplier(), 0.0);
    }

    #[test]
    fn rejects_out_of_range_multiplier() {
        let resp = LlmResponse {
            pull: false,
            risk_multiplier: 1.5,
            ttl_ms: 1000,
            reason_code: "test".into(),
        };
        assert!(validate_response(&resp).is_err());

        let resp2 = LlmResponse {
            risk_multiplier: -0.1,
            ..resp.clone()
        };
        assert!(validate_response(&resp2).is_err());
    }

    #[test]
    fn rejects_zero_ttl() {
        let resp = LlmResponse {
            pull: false,
            risk_multiplier: 0.5,
            ttl_ms: 0,
            reason_code: "test".into(),
        };
        assert!(validate_response(&resp).is_err());
    }

    #[test]
    fn rejects_empty_reason_code() {
        let resp = LlmResponse {
            pull: false,
            risk_multiplier: 0.5,
            ttl_ms: 1000,
            reason_code: "".into(),
        };
        assert!(validate_response(&resp).is_err());
    }

    #[test]
    fn rejects_oversized_reason_code() {
        let resp = LlmResponse {
            pull: false,
            risk_multiplier: 0.5,
            ttl_ms: 1000,
            reason_code: "a".repeat(65),
        };
        assert!(validate_response(&resp).is_err());
    }

    #[test]
    fn boundary_multiplier_values_accepted() {
        // Exactly 0.0 and 1.0 should be valid
        let resp_zero = LlmResponse {
            pull: false,
            risk_multiplier: 0.0,
            ttl_ms: 1000,
            reason_code: "min".into(),
        };
        assert!(validate_response(&resp_zero).is_ok());

        let resp_one = LlmResponse {
            pull: false,
            risk_multiplier: 1.0,
            ttl_ms: 1000,
            reason_code: "max".into(),
        };
        assert!(validate_response(&resp_one).is_ok());
    }

    #[test]
    fn llm_response_serialization_roundtrip() {
        let resp = LlmResponse {
            pull: true,
            risk_multiplier: 0.42,
            ttl_ms: 10000,
            reason_code: "test_roundtrip".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: LlmResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pull, resp.pull);
        assert_eq!(parsed.risk_multiplier, resp.risk_multiplier);
        assert_eq!(parsed.ttl_ms, resp.ttl_ms);
        assert_eq!(parsed.reason_code, resp.reason_code);
    }
}
