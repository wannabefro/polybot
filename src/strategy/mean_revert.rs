// Behavioral mean reversion (small allocation).
// Deep markets only, 0.5% NAV cap, time-stop + price-stop.
//
// Implementation deferred until rebate-mm is live and validated.
// Key constraints from plan:
// - Only markets with 24h volume > mean_revert_min_volume_24h
// - Max position: mean_revert_max_nav_frac * NAV
// - Time-stop: exit after N hours if not profitable
// - Price-stop: exit at X% loss
