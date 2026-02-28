use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// Risk signal emitted by the LLM intelligence loop.
///
/// The hot path reads this via `watch::Receiver` — zero network calls,
/// zero allocations on read (just an Arc clone).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskSignal {
    /// If true, pull all quotes and go flat.
    pub pull: bool,
    /// Multiplier applied to position sizing (0.0 = no risk, 1.0 = full).
    pub risk_multiplier: f64,
    /// Time-to-live in milliseconds; after expiry, revert to defaults.
    pub ttl_ms: u64,
    /// Machine-readable reason code (e.g., "news_negative", "volatility_spike").
    pub reason_code: String,
    /// When this signal was produced.
    #[serde(skip)]
    pub created_at: Option<Instant>,
}

impl Default for RiskSignal {
    fn default() -> Self {
        Self {
            pull: false,
            risk_multiplier: 1.0,
            ttl_ms: 60_000,
            reason_code: "default".into(),
            created_at: Some(Instant::now()),
        }
    }
}

impl RiskSignal {
    /// Check if this signal has expired based on its TTL.
    #[allow(dead_code)]
    pub fn is_expired(&self) -> bool {
        match self.created_at {
            Some(t) => t.elapsed() > Duration::from_millis(self.ttl_ms),
            None => true,
        }
    }

    /// Effective risk multiplier, accounting for expiry and pull.
    /// Returns 0.0 if pull is true, reverts to 1.0 if expired.
    #[allow(dead_code)]
    pub fn effective_multiplier(&self) -> f64 {
        if self.pull {
            return 0.0;
        }
        if self.is_expired() {
            return 1.0;
        }
        self.risk_multiplier.clamp(0.0, 1.0)
    }
}

/// Sender half — owned by the intelligence loop.
pub type SignalSender = watch::Sender<Arc<RiskSignal>>;

/// Receiver half — cloned into every hot-path component.
pub type SignalReceiver = watch::Receiver<Arc<RiskSignal>>;

/// Create a new signal bridge with default (neutral) signal.
pub fn create() -> (SignalSender, SignalReceiver) {
    watch::channel(Arc::new(RiskSignal::default()))
}

/// Read the current signal (non-blocking, zero-copy via Arc).
pub fn read(rx: &SignalReceiver) -> Arc<RiskSignal> {
    rx.borrow().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn default_signal_is_neutral() {
        let sig = RiskSignal::default();
        assert!(!sig.pull);
        assert_eq!(sig.risk_multiplier, 1.0);
        assert_eq!(sig.effective_multiplier(), 1.0);
        assert!(!sig.is_expired());
    }

    #[test]
    fn pull_signal_zeroes_multiplier() {
        let sig = RiskSignal {
            pull: true,
            risk_multiplier: 0.8,
            ..Default::default()
        };
        assert_eq!(sig.effective_multiplier(), 0.0);
    }

    #[test]
    fn expired_signal_reverts_to_full() {
        let sig = RiskSignal {
            pull: false,
            risk_multiplier: 0.3,
            ttl_ms: 0, // immediate expiry
            reason_code: "test".into(),
            created_at: Some(Instant::now() - Duration::from_secs(1)),
        };
        assert!(sig.is_expired());
        assert_eq!(sig.effective_multiplier(), 1.0);
    }

    #[test]
    fn multiplier_clamped_to_unit_range() {
        let sig = RiskSignal {
            risk_multiplier: 1.5,
            ..Default::default()
        };
        assert_eq!(sig.effective_multiplier(), 1.0);

        let sig2 = RiskSignal {
            risk_multiplier: -0.5,
            ..Default::default()
        };
        assert_eq!(sig2.effective_multiplier(), 0.0);
    }

    #[test]
    fn bridge_send_receive() {
        let (tx, rx) = create();

        // Default signal
        let sig = read(&rx);
        assert!(!sig.pull);
        assert_eq!(sig.effective_multiplier(), 1.0);

        // Send a pull signal
        let pull = Arc::new(RiskSignal {
            pull: true,
            risk_multiplier: 0.0,
            ttl_ms: 5000,
            reason_code: "news_negative".into(),
            created_at: Some(Instant::now()),
        });
        tx.send(pull).unwrap();

        let sig = read(&rx);
        assert!(sig.pull);
        assert_eq!(sig.effective_multiplier(), 0.0);
        assert_eq!(sig.reason_code, "news_negative");
    }

    #[test]
    fn multiple_receivers_see_same_signal() {
        let (tx, rx1) = create();
        let rx2 = rx1.clone();
        let rx3 = rx1.clone();

        let sig = Arc::new(RiskSignal {
            risk_multiplier: 0.5,
            reason_code: "vol_spike".into(),
            ..Default::default()
        });
        tx.send(sig).unwrap();

        assert_eq!(read(&rx1).risk_multiplier, 0.5);
        assert_eq!(read(&rx2).risk_multiplier, 0.5);
        assert_eq!(read(&rx3).risk_multiplier, 0.5);
    }

    #[test]
    fn signal_none_created_at_is_expired() {
        let sig = RiskSignal {
            created_at: None,
            ..Default::default()
        };
        assert!(sig.is_expired());
        assert_eq!(sig.effective_multiplier(), 1.0);
    }
}
