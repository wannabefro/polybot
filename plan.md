# Polymarket Low-Risk Trading Bot: Tightened Plan (v2)

## 1. Hard Gate: Compliance + Viability
- Run `GET https://polymarket.com/api/geoblock` at startup and every 15 minutes.
- If `blocked=true`, do not place orders.
- As of current docs, `US` is listed as blocked for order placement.
- Abort startup if this gate fails.

## 2. Architecture (Strict Separation)
- Hot path (Rust, no external AI calls): websocket ingest, local book state, quoting, order submit/cancel, risk checks.
- Intelligence loop (async background): news/comments ingestion + LLM inference.
- Bridge: hot path reads a non-blocking shared signal (`watch`/atomics/lock-minimized state). No network call in execution loop.
- Rule: LLM can only adjust risk posture (`PULL`, `RISK_MULTIPLIER`), not direct order prices.

## 3. Dependencies
- `polymarket-client-sdk` with features: `["clob","gamma","ws","heartbeats","data"]`.
- `tokio`, `reqwest`, `serde`, `serde_json`, `tracing`.
- Keep binary lean; no heavy ML in-process.

## 4. Market Discovery Engine
- Primary feed: CLOB `GET /sampling-markets` and `GET /simplified-markets`.
- Filter criteria:
  - `active=true`, `accepting_orders=true`, `closed=false`, `archived=false`.
  - Rewards present (`rewards.rates`, `rewards.min_size`, `rewards.max_spread`).
  - Fee-enabled cohorts: 15-minute crypto, 5-minute crypto, NCAAB, Serie A.
- For neg-risk detection, use market/event flags (`negRisk`, plus augmented flags from Gamma metadata).
- Exclude augmented neg-risk `"Other"` outcome from tradable universe.
- Neg-risk pair rule: always quote both sides of a neg-risk pair simultaneously; halt quoting on the pair if either side's quote is stale or cancelled.
- Recompute ranked universe every 60 seconds.

## 5. Market Data + State Integrity
- Subscribe to `wss://ws-subscriptions-clob.polymarket.com/ws/market` with `asset_ids`.
- Maintain per-asset local book using `book` + `price_change`.
- Track `timestamp` and `hash` on updates.
- Resync trigger conditions:
  - websocket reconnect,
  - stale feed (`>1500ms` no update on active symbols),
  - timestamp regression,
  - hash inconsistency vs periodic REST snapshot.
- On trigger: pause quoting, refresh snapshot, resume only after state healthy.
- Post-reconnect resync: stagger/batch REST snapshot requests to avoid blowing rate-limit budget on thundering-herd reconnects.

## 6. Auth + Order Pipeline
- Boot flow: L1 signer -> derive/create L2 credentials -> cache securely.
- Every order must satisfy:
  - correct `tickSize`,
  - correct `feeRateBps` (fetched dynamically),
  - `negRisk=true` when required.
- Maker strategy orders: `GTC/GTD + postOnly=true`.
- Emergency hedge/unwind: `FAK` or `FOK` (never with `postOnly`).

## 7. Heartbeat + Session Safety
- Send heartbeat every 5 seconds with rolling `heartbeat_id`.
- If no valid heartbeat window (<10s + small buffer) can be guaranteed:
  - immediate `cancel-all`,
  - switch to `SAFE_PAUSE`,
  - resume only after heartbeat healthy again.
- If heartbeat returns invalid/expired ID, replace with server-provided ID and retry.

## 8. Position Reconciliation
- Poll `GET /positions` every 30–60 seconds.
- Compare local position state against API response.
- On mismatch: immediately halt quoting, log discrepancy, attempt to reconcile.
- Resume only after local state matches API state.
- This catches drift from partial fills, network issues, or missed fill confirmations.

## 9. Rate-Limit Budgeting
- Keep REST off hot path except submit/cancel and periodic integrity checks.
- Use local token-bucket guards at ~70% of published limits.
- Explicitly model burst + sustained limits for `POST /order`, `DELETE /order`, `DELETE /cancel-all`.
- Backoff/jitter on throttle responses; never spin-retry.

## 10. Strategies (Conservative Defaults)
- Rebate MM (primary):
  - two-sided quotes around adjusted midpoint,
  - spread and size constrained by market `rewards.max_spread` and `rewards.min_size`,
  - inventory-skewed quoting when imbalanced.
- Behavioral mean reversion (small allocation):
  - only in deep markets (minimum 24h volume threshold required),
  - max 0.5% NAV per position (tighter than general market cap),
  - capped tranche entries,
  - hard time-stop and price-stop.
- Sponsored/reward capture:
  - only where reward density is high and depth is low,
  - if one leg fills, immediately neutralize with `FAK` hedge or flatten.
  - hedge latency SLA: if hedge does not fill within 500ms, escalate to `cancel-all` + flatten entire position. A partial fill with no hedge is the highest-risk state.

## 11. Risk Guardrails (Required)
- Max notional per market: 2% NAV.
- Max gross open exposure: 25% NAV.
- Max one-sided inventory per outcome: 1% NAV.
- Daily loss stop: 3% NAV -> cancel-all + halt until next UTC day.
- Consecutive cancel failures >=3 -> halt.
- Repeated 425 restart responses -> pause and exponential retry.
- No fresh market data -> no quoting.

## 12. LLM Loop Contract
- Input: comments/news only.
- Output schema: `{"pull": bool, "risk_multiplier": 0..1, "ttl_ms": int, "reason_code": string}`.
- Timeout: 1500ms per request.
- If timeout/invalid schema: ignore signal and keep current risk state.
- Signal TTL enforced; stale signal auto-expires to neutral.

## 13. Operations + Rollout
- Run as `systemd` service with automatic restart.
- Structured logs + metrics: quote age, fill rate, slippage, cancel latency, heartbeat age, throttle count, PnL, rebate accrual.
- Rollout:
  - Phase 1: paper mode (no live orders).
  - Phase 2: micro-size live (10% intended size).
  - Phase 3: full size only after 7 stable days.

## Sources
- https://docs.polymarket.com/api-reference/rate-limits
- https://docs.polymarket.com/trading/orders/overview
- https://docs.polymarket.com/developers/CLOB/authentication
- https://docs.polymarket.com/market-data/websocket/market-channel
- https://docs.polymarket.com/developers/CLOB/websocket/market-channel-migration-guide
- https://docs.polymarket.com/advanced/neg-risk
- https://docs.polymarket.com/market-makers/maker-rebates
- https://docs.polymarket.com/api-reference/markets/get-simplified-markets
- https://docs.polymarket.com/api-reference/markets/get-sampling-markets
- https://docs.polymarket.com/trading/matching-engine
- https://docs.polymarket.com/api-reference/geoblock
- https://github.com/Polymarket/rs-clob-client
