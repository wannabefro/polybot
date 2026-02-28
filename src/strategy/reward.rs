// Sponsored/reward capture strategy.
// High reward density + low depth. FAK hedge on fill,
// 500ms hedge SLA with cancel-all escalation.
//
// Implementation deferred until rebate-mm is live.
// Key constraints from plan:
// - Hedge latency SLA: 500ms timeout
// - On timeout: cancel-all + flatten position
// - Only enter when reward_daily_rate justifies the spread risk
