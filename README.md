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

## Deployment

### Local (development)

```bash
cp deploy/.env.example .env.local
# Edit .env.local with your private key and settings
POLYBOT_PAPER_MODE=false cargo run --release
```

### Server (systemd)

**1. Build the binary** (on your dev machine or CI):

```bash
# Native build (run on the target server)
cargo build --release

# Or cross-compile for Linux x86_64 from macOS
cargo build --release --target x86_64-unknown-linux-gnu
```

**2. Install on the server:**

```bash
# With pre-built binary
sudo ./deploy/install.sh ./target/release/polybot

# Or build on server (requires Rust toolchain)
sudo ./deploy/install.sh
```

**3. Configure:**

```bash
sudo nano /opt/polybot/.env
# Set POLYBOT_PRIVATE_KEY, POLYBOT_PAPER_MODE=false, POLYBOT_NAV_USDC, etc.
# For geo-restricted regions, set NORDVPN_* variables (see .env.example)
```

**4. Start:**

```bash
sudo systemctl start polybot
sudo systemctl enable polybot   # auto-start on boot
journalctl -u polybot -f        # tail logs
```

### SOCKS5 Proxy (geo-restricted regions)

If running from a region blocked by Polymarket, configure a SOCKS5 proxy in `.env`:

```bash
NORDVPN_USER=your_service_username
NORDVPN_PASS=your_service_password
NORDVPN_HOST=se.socks.nordhold.net   # Sweden — Finland also works
NORDVPN_PORT=1080
```

The bot sets `ALL_PROXY` before creating HTTP clients. All REST API traffic routes through the proxy. WebSocket connections are direct (not geo-blocked).

> **Note:** US, Netherlands, and most EU countries are blocked. Sweden and Finland are confirmed working.

### Systemd service details

- Sends `SIGINT` on stop (clean shutdown with order cancellation)
- Restarts on failure with 30s backoff
- Runs as dedicated `polybot` user with filesystem hardening
- Logs to journald (`journalctl -u polybot`)

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
│   ├── decay.rs         # Time-decay buying strategy
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

## Environment Variables

All variables use the `POLYBOT_` prefix. Only `POLYBOT_PRIVATE_KEY` is required.

| Variable | Default | Description |
|----------|---------|-------------|
| `POLYBOT_PRIVATE_KEY` | *(required)* | Hex-encoded Ethereum private key |
| `POLYBOT_PAPER_MODE` | `true` | Paper trading (no real orders) |
| `POLYBOT_NAV_USDC` | `1000.0` | Starting NAV in USDC |
| `POLYBOT_CLOB_HOST` | `https://clob.polymarket.com` | CLOB API base URL |
| `POLYBOT_GAMMA_HOST` | `https://gamma-api.polymarket.com` | Gamma API base URL |
| `POLYBOT_CHAIN_ID` | `137` | Polygon chain ID |
| `POLYBOT_MAX_NOTIONAL_PCT` | `0.02` | Max notional per market (fraction of NAV) |
| `POLYBOT_MAX_GROSS_PCT` | `0.25` | Max gross exposure (fraction of NAV) |
| `POLYBOT_MAX_INVENTORY_PCT` | `0.01` | Max one-sided inventory (fraction of NAV) |
| `POLYBOT_DAILY_LOSS_PCT` | `0.03` | Daily loss stop (fraction of NAV) |
| `POLYBOT_HEARTBEAT_SECS` | `5` | Heartbeat interval (seconds) |
| `POLYBOT_GEOBLOCK_POLL_SECS` | `900` | Geoblock re-check interval (seconds) |
| `POLYBOT_DISCOVERY_SECS` | `60` | Market discovery interval (seconds) |
| `POLYBOT_RECON_SECS` | `45` | Position reconciliation interval (seconds) |
| `POLYBOT_STALE_FEED_MS` | `1500` | Book staleness threshold (milliseconds) |
| `POLYBOT_MR_MAX_NAV_PCT` | `0.005` | Mean-revert max position (fraction of NAV) |
| `POLYBOT_MR_MIN_VOL_24H` | `10000.0` | Mean-revert minimum 24h volume |
| `POLYBOT_HEDGE_TIMEOUT_MS` | `500` | Hedge SLA timeout (milliseconds) |
| `POLYBOT_RATE_LIMIT_PS` | `70.0` | Rate limit (orders per second) |
| `POLYBOT_LLM_POLL_SECS` | `10` | LLM signal poll interval (seconds) |
| `POLYBOT_LLM_ENDPOINT` | *(none)* | Optional LLM inference endpoint |
| `POLYBOT_METRICS_SECS` | `30` | Metrics log interval (seconds) |
| `POLYBOT_MM_MIN_SIZE` | `5.0` | Minimum quote size (USDC) |
| `POLYBOT_NR_STALE_SECS` | `10` | Neg-risk pair staleness threshold (seconds) |
| `POLYBOT_QUOTE_TICK_SECS` | `5` | Quoting tick interval (seconds) |

See `plan.md` for full architecture documentation.
