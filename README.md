# Polybot — Polymarket Low-Risk Trading Bot

A Rust-based market-making bot for Polymarket prediction markets, focused on rebate capture and conservative risk management.

## Architecture

- **Hot path** (Rust, zero external calls): WebSocket ingest → local book → quoting → order submit/cancel → risk checks
- **Intelligence loop** (async background): news/comments → LLM inference → risk signal
- **Bridge**: hot path reads signals via non-blocking `watch`/atomics

## Quick Start

```bash
# Configure (paper mode by default)
export POLYBOT_PRIVATE_KEY="your_hex_key"
export POLYBOT_PAPER_MODE=true
export POLYBOT_NAV_USDC=1000.0

# Build and run
cargo build --release
cargo run --release
```

## Project Structure

```
src/
├── main.rs              # Entry point, service orchestration
├── config.rs            # Environment-based configuration
├── geoblock.rs          # Compliance gate (geoblock check)
├── auth.rs              # L1/L2 credential management
├── market/
│   ├── discovery.rs     # Market discovery + filtering
│   ├── book.rs          # Local orderbook state
│   └── ws.rs            # WebSocket feed handler
├── order/
│   ├── pipeline.rs      # Order construction + validation
│   └── heartbeat.rs     # Heartbeat session management
├── risk/
│   ├── guardrails.rs    # Risk limits enforcement
│   ├── position.rs      # Position reconciliation
│   └── rate_limit.rs    # Token-bucket rate limiter
├── strategy/
│   ├── rebate_mm.rs     # Rebate market-making (primary)
│   ├── mean_revert.rs   # Behavioral mean reversion
│   └── reward.rs        # Reward/sponsored capture
├── intelligence/
│   ├── llm.rs           # LLM inference loop
│   └── signal.rs        # Shared signal bridge
└── ops/
    ├── metrics.rs       # Structured metrics
    └── paper.rs         # Paper trading mode
```

## Risk Limits

| Limit | Value |
|-------|-------|
| Max notional per market | 2% NAV |
| Max gross exposure | 25% NAV |
| Max one-sided inventory | 1% NAV |
| Daily loss stop | 3% NAV |

See `plan.md` for full architecture documentation.
