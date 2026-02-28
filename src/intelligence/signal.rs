// Shared signal bridge between intelligence loop and hot path.
// Non-blocking reads via tokio::sync::watch or atomics.
// Signal: { pull: bool, risk_multiplier: 0..1, ttl_ms, reason_code }
