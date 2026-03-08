//! Late-stage time-decay strategy ("Penny Collector").
//!
//! Scans for binary markets resolving within a configurable window (default 24h)
//! where one outcome trades at >= min_price (default 0.90). Buys shares via IOC
//! and holds to resolution. Strict per-market and total capital limits.

use chrono::{DateTime, Utc};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;

/// Tracks deployed capital and active decay positions.
#[derive(Debug)]
pub struct DecayTracker {
    /// condition_id → (token_id, size bought, cost_basis)
    pub positions: HashMap<String, DecayPosition>,
    /// token_ids already held on-chain (bootstrapped from positions API).
    /// Prevents duplicate buys after restart.
    held_tokens: std::collections::HashSet<String>,
    /// condition_ids already held on-chain.
    /// Prevents buying the opposite outcome of a held market.
    held_conditions: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DecayPosition {
    pub token_id: String,
    pub condition_id: String,
    pub outcome: String,
    pub size: Decimal,
    pub cost_basis: Decimal,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
}

impl DecayTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            held_tokens: std::collections::HashSet::new(),
            held_conditions: std::collections::HashSet::new(),
        }
    }

    /// Seed with full position data from the on-chain positions API.
    /// Tracks both token_ids and condition_ids to prevent any duplicate buys.
    pub fn seed_from_remote(&mut self, positions: &[crate::risk::position::RemotePosition]) {
        for pos in positions {
            self.held_tokens.insert(pos.token_id.clone());
            self.held_conditions.insert(pos.condition_id.clone());
        }
        if !self.held_tokens.is_empty() {
            info!(
                tokens = self.held_tokens.len(),
                conditions = self.held_conditions.len(),
                "decay: seeded held positions from on-chain"
            );
        }
    }

    /// Total USDC currently deployed in decay positions.
    pub fn deployed_capital(&self) -> Decimal {
        self.positions.values().map(|p| p.cost_basis).sum()
    }

    /// Record a fill from a decay buy.
    pub fn record_fill(
        &mut self,
        condition_id: &str,
        token_id: &str,
        outcome: &str,
        size: Decimal,
        price: Decimal,
        neg_risk: bool,
        fee_rate_bps: Decimal,
    ) {
        let cost = size * price;
        let entry = self
            .positions
            .entry(condition_id.to_string())
            .or_insert_with(|| DecayPosition {
                token_id: token_id.to_string(),
                condition_id: condition_id.to_string(),
                outcome: outcome.to_string(),
                size: Decimal::ZERO,
                cost_basis: Decimal::ZERO,
                neg_risk,
                fee_rate_bps,
            });
        entry.size += size;
        entry.cost_basis += cost;
        self.held_tokens.insert(token_id.to_string());
    }

    /// Remove positions for markets that are no longer accepting orders (resolved).
    pub fn cleanup_resolved(&mut self, markets: &[TradableMarket]) {
        let active_ids: std::collections::HashSet<&str> =
            markets.iter().map(|m| m.condition_id.as_str()).collect();
        let before = self.positions.len();
        self.positions
            .retain(|cid, _| active_ids.contains(cid.as_str()));
        let removed = before - self.positions.len();
        if removed > 0 {
            info!(removed, "decay: cleaned up resolved positions");
        }
    }

    /// Number of active positions.
    pub fn position_count(&self) -> usize {
        self.positions.len()
    }
}

/// A candidate market for a decay buy.
#[derive(Debug)]
pub struct DecayCandidate {
    pub condition_id: String,
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
    pub hours_to_end: f64,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
    pub min_order_size: Decimal,
    /// Total size available at or below the candidate price (sum of ask levels ≤ price).
    pub available_size: Decimal,
    /// Whether this is a sports market (uses shorter window and smaller max bet).
    pub is_sports: bool,
    /// Edge-per-hour score (higher = better opportunity).
    pub score: f64,
}

/// Score a market opportunity for high-frequency, low-profit trading.
///
/// Scoring formula: edge * turnover_rate * bonuses
/// - edge: 1 - price (what you pay for certainty)
/// - turnover_rate: 1 / (abs_hours + 0.1) - faster resolution = better
/// - sports_in_progress_bonus: 2x for games that have started
fn score_opportunity(
    price: Decimal,
    secs_to_resolution: i64,
    is_sports: bool,
    is_game_in_progress: bool,
) -> f64 {
    let price_f: f64 = price.try_into().unwrap_or(0.95);

    // Edge: what you pay for near-certainty
    let edge = 1.0 - price_f;

    // Use absolute time for turnover calculation
    // For games in progress, this is how long until resolution (estimated)
    // For pre-game, this is time until game starts
    let abs_hours = (secs_to_resolution.abs() as f64) / 3600.0;

    // Turnover rate: faster resolution = more capital efficient
    // Add small constant to avoid division by zero
    let turnover_rate = 1.0 / (abs_hours + 0.1);

    // Base score
    let mut score = edge * turnover_rate;

    // Sports in progress bonus: games that have started are much more predictable
    // Pre-game odds move a lot; in-game odds converge to outcome
    if is_sports && is_game_in_progress {
        score *= 2.0;
    }

    // Price certainty bonus: higher price = more certain outcome
    if price_f >= 0.95 {
        score *= 1.2;
    } else if price_f >= 0.92 {
        score *= 1.1;
    }

    score
}

/// Check if a market is a sports market based on tags.
fn is_sports_market(market: &TradableMarket) -> bool {
    market.tags.iter().any(|t| {
        let lower = t.to_lowercase();
        lower.contains("sports")
            || lower.contains("football")
            || lower.contains("basketball")
            || lower.contains("baseball")
            || lower.contains("hockey")
            || lower.contains("soccer")
            || lower.contains("tennis")
            || lower.contains("golf")
            || lower.contains("boxing")
            || lower.contains("mma")
            || lower.contains("nfl")
            || lower.contains("nba")
            || lower.contains("mlb")
            || lower.contains("nhl")
            || lower.contains("ncaa")
    })
}

/// Scan tradable markets for decay candidates using edge-per-hour scoring.
///
/// For sports markets, allows games that are in progress (end_date passed)
/// up to a maximum game duration (typically 3-4 hours).
pub fn scan_candidates(
    markets: &[TradableMarket],
    books: &BookStore,
    config: &Config,
    now: DateTime<Utc>,
) -> Vec<DecayCandidate> {
    if !config.decay_enabled {
        return vec![];
    }

    let default_min_price = Decimal::try_from(config.decay_min_price).unwrap_or(dec!(0.85));
    let sports_min_price = Decimal::try_from(config.decay_sports_min_price).unwrap_or(dec!(0.90));
    let default_window_secs = (config.decay_window_hours * 3600.0) as i64;
    let sports_window_secs = (config.decay_sports_window_hours * 3600.0) as i64;
    // Max game duration for in-progress sports (3.5 hours = 12600 seconds)
    const MAX_GAME_DURATION_SECS: i64 = 12600;

    let mut candidates = Vec::new();
    let mut skip_non_binary = 0u32;
    let mut skip_no_end_date = 0u32;
    let mut skip_outside_window = 0u32;
    let mut skip_tag_excluded = 0u32;
    let mut skip_low_price = 0u32;
    let mut skip_no_book = 0u32;
    let mut skip_no_ask = 0u32;
    // Sports-specific diagnostics
    let mut sports_total = 0u32;
    let mut sports_in_progress = 0u32;
    let mut sports_pre_game = 0u32;
    let mut sports_skip_window = 0u32;
    let mut sports_skip_price = 0u32;
    let mut sports_candidates = 0u32;

    for market in markets {
        // Binary markets only (exactly 2 outcomes)
        if market.tokens.len() != 2 {
            skip_non_binary += 1;
            continue;
        }

        // Must have an end date
        let end_date = match market.end_date {
            Some(d) => d,
            None => {
                skip_no_end_date += 1;
                continue;
            }
        };
        let secs_remaining = (end_date - now).num_seconds();

        // Determine if sports market
        let is_sports = is_sports_market(market);
        if is_sports {
            sports_total += 1;
        }

        // Window check - different logic for sports vs normal
        let is_game_in_progress =
            is_sports && secs_remaining < 0 && secs_remaining > -MAX_GAME_DURATION_SECS;
        let is_within_window = if is_sports {
            if secs_remaining > 0 {
                // Pre-game: must be within sports window
                sports_pre_game += 1;
                secs_remaining <= sports_window_secs
            } else {
                // In-progress: game must have started recently
                sports_in_progress += 1;
                secs_remaining > -MAX_GAME_DURATION_SECS
            }
        } else {
            // Normal markets: must end within window and not have ended yet
            secs_remaining > 0 && secs_remaining <= default_window_secs
        };

        if !is_within_window {
            skip_outside_window += 1;
            if is_sports {
                sports_skip_window += 1;
            }
            continue;
        }

        // Tag exclusion (case-insensitive)
        let excluded = market.tags.iter().any(|t| {
            let lower = t.to_lowercase();
            config
                .decay_excluded_tags
                .iter()
                .any(|ex| lower.contains(ex))
        });
        if excluded {
            skip_tag_excluded += 1;
            continue;
        }

        // Use sports-specific min price for sports markets
        let min_price = if is_sports {
            sports_min_price
        } else {
            default_min_price
        };

        // Find the highest-priced token that exceeds min_price
        let best_token = market
            .tokens
            .iter()
            .filter(|t| t.price >= min_price)
            .max_by_key(|t| t.price);

        let token = match best_token {
            Some(t) => t,
            None => {
                skip_low_price += 1;
                if is_sports {
                    sports_skip_price += 1;
                }
                continue;
            }
        };

        // Verify there's actually an ask available at a reasonable price
        let book = match books.get(&token.token_id) {
            Some(b) => b,
            None => {
                skip_no_book += 1;
                continue;
            }
        };
        let best_ask = match book.asks.best() {
            Some(a) => a.price,
            None => {
                skip_no_ask += 1;
                continue;
            }
        };
        if best_ask > Decimal::ONE || best_ask < min_price {
            skip_low_price += 1;
            continue;
        }

        // Sum available liquidity across ask levels up to best_ask + 2 ticks.
        // For decay, we're willing to pay slightly above best ask to ensure fill.
        let sweep_ceiling = (best_ask + dec!(0.02)).min(Decimal::ONE);
        let mut sweep_size = Decimal::ZERO;
        let mut worst_price = best_ask;
        for level in &book.asks.levels {
            if level.price > sweep_ceiling {
                break;
            }
            sweep_size += level.size;
            worst_price = level.price;
        }

        // For in-progress games, estimate time to resolution
        // (game already started, so remaining time is less than full game duration)
        let secs_to_resolution = if is_game_in_progress {
            // Estimate remaining time: max duration - elapsed time
            let elapsed = -secs_remaining;
            (MAX_GAME_DURATION_SECS - elapsed).max(300) // at least 5 min remaining
        } else {
            secs_remaining
        };

        let hours_to_end = secs_remaining as f64 / 3600.0;

        // Score the opportunity
        let score = score_opportunity(
            worst_price,
            secs_to_resolution,
            is_sports,
            is_game_in_progress,
        );

        candidates.push(DecayCandidate {
            condition_id: market.condition_id.clone(),
            token_id: token.token_id.clone(),
            outcome: token.outcome.clone(),
            price: worst_price,
            hours_to_end,
            neg_risk: market.neg_risk,
            fee_rate_bps: market.maker_fee_bps,
            min_order_size: market.min_order_size,
            available_size: sweep_size,
            is_sports,
            score,
        });
        if is_sports {
            sports_candidates += 1;
        }
    }

    if candidates.is_empty() && !markets.is_empty() {
        info!(
            total = markets.len(),
            skip_non_binary,
            skip_no_end_date,
            skip_outside_window,
            skip_tag_excluded,
            skip_low_price,
            skip_no_book,
            skip_no_ask,
            min_price = %default_min_price,
            sports_min_price = %sports_min_price,
            window_hours = config.decay_window_hours,
            sports_window_hours = config.decay_sports_window_hours,
            sports_total,
            sports_in_progress,
            sports_pre_game,
            sports_skip_window,
            sports_skip_price,
            "🔍 decay: no candidates — filter breakdown"
        );
    } else if !candidates.is_empty() {
        // Sort by score descending (best opportunity first)
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        info!(
            found = candidates.len(),
            total = markets.len(),
            skip_no_end_date,
            skip_outside_window,
            skip_low_price,
            best_price = %candidates[0].price,
            best_hours = format!("{:.1}", candidates[0].hours_to_end),
            best_score = format!("{:.3}", candidates[0].score),
            min_price = %default_min_price,
            sports_min_price = %sports_min_price,
            window_hours = config.decay_window_hours,
            sports_window_hours = config.decay_sports_window_hours,
            sports_total,
            sports_in_progress,
            sports_candidates,
            sports_skip_window,
            sports_skip_price,
            "🔍 decay: candidates found"
        );
    }

    candidates
}

/// Build an IOC buy intent for a decay candidate with strict sizing.
pub fn evaluate_decay_buy(
    candidate: &DecayCandidate,
    config: &Config,
    tracker: &DecayTracker,
    free_balance: Decimal,
) -> Option<OrderIntent> {
    // Use sports-specific max bet for sports markets
    let max_bet = if candidate.is_sports {
        Decimal::try_from(config.decay_sports_max_bet_usdc).unwrap_or(dec!(5.0))
    } else {
        Decimal::try_from(config.decay_max_bet_usdc).unwrap_or(dec!(15.0))
    };
    let nav_cap =
        Decimal::try_from(config.nav_usdc * config.decay_nav_fraction).unwrap_or(dec!(100.0));

    // Check total capital deployed
    let deployed = tracker.deployed_capital();
    let remaining_budget = nav_cap - deployed;
    if remaining_budget <= Decimal::ZERO {
        info!(deployed = %deployed, nav_cap = %nav_cap, "decay: skip — capital budget exhausted");
        return None;
    }

    // Already have a position in this market?
    if tracker.positions.contains_key(&candidate.condition_id) {
        debug!(condition_id = %candidate.condition_id, "decay: skip — duplicate position");
        return None;
    }

    // Already hold this token or any token in this condition on-chain?
    if tracker.held_tokens.contains(&candidate.token_id)
        || tracker.held_conditions.contains(&candidate.condition_id)
    {
        debug!(
            token_id = %candidate.token_id,
            condition_id = %candidate.condition_id,
            "decay: skip — already held on-chain"
        );
        return None;
    }

    // Per-market cap, also capped by actual free USDC balance
    let usdc_to_spend = max_bet.min(remaining_budget).min(free_balance);

    // Calculate size: spend / price, capped by available liquidity
    let desired_size = (usdc_to_spend / candidate.price)
        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
    let size = desired_size
        .min(candidate.available_size)
        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);

    // Validate both size and USDC cost are above minimums.
    // Cost truncated to 2dp (CLOB requirement) must also be positive.
    let cost =
        (size * candidate.price).round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
    if size < candidate.min_order_size || size <= Decimal::ZERO || cost <= Decimal::ZERO {
        info!(
            condition_id = %candidate.condition_id,
            size = %size,
            min_order_size = %candidate.min_order_size,
            price = %candidate.price,
            usdc_to_spend = %usdc_to_spend,
            "decay: skip — size below minimum"
        );
        return None;
    }

    info!(
        condition_id = %candidate.condition_id,
        outcome = %candidate.outcome,
        price = %candidate.price,
        size = %size,
        available = %candidate.available_size,
        hours_to_end = candidate.hours_to_end,
        "decay: placing FOK buy"
    );

    Some(OrderIntent {
        token_id: candidate.token_id.clone(),
        side: Side::Buy,
        price: candidate.price,
        size,
        order_type: OrderType::FOK,
        post_only: false,
        neg_risk: candidate.neg_risk,
        fee_rate_bps: candidate.fee_rate_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::market::book::BookStore;
    use crate::market::discovery::{TokenInfo, TradableMarket};
    use chrono::Duration as ChronoDuration;

    fn make_market(
        condition_id: &str,
        end_date: Option<DateTime<Utc>>,
        tokens: Vec<TokenInfo>,
        tags: Vec<String>,
        neg_risk: bool,
    ) -> TradableMarket {
        TradableMarket {
            condition_id: condition_id.to_string(),
            question: "Test market?".to_string(),
            tokens,
            neg_risk,
            neg_risk_market_id: None,
            min_tick_size: dec!(0.01),
            min_order_size: dec!(1.0),
            maker_fee_bps: dec!(0),
            rewards_active: false,
            rewards_max_spread: None,
            rewards_min_size: None,
            volume_24h: 0.0,
            tags,
            end_date,
        }
    }

    fn make_tokens(yes_price: &str, no_price: &str) -> Vec<TokenInfo> {
        vec![
            TokenInfo {
                token_id: "tok_yes".to_string(),
                outcome: "Yes".to_string(),
                price: yes_price.parse().unwrap(),
            },
            TokenInfo {
                token_id: "tok_no".to_string(),
                outcome: "No".to_string(),
                price: no_price.parse().unwrap(),
            },
        ]
    }

    fn make_books_with_ask(token_id: &str, ask_price: Decimal) -> BookStore {
        use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
        use polymarket_client_sdk::types::{B256, U256};
        let books = BookStore::new();
        let level = OrderBookLevel::builder()
            .price(ask_price)
            .size(dec!(100))
            .build();
        let update = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(1000)
            .bids(vec![])
            .asks(vec![level])
            .hash("test".into())
            .build();
        books.apply(token_id, &update);
        books
    }

    #[test]
    fn scan_finds_candidate_within_window() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].outcome, "Yes");
        assert_eq!(candidates[0].price, dec!(0.95));
    }

    #[test]
    fn scan_skips_market_outside_window() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(200); // beyond 168h window
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_low_price() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.80", "0.20"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.80));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_excluded_tags() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.95", "0.05"),
            vec!["Crypto".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_accepts_neg_risk() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], true);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn scan_skips_no_end_date() {
        let now = Utc::now();
        let market = make_market("c1", None, make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_multi_outcome() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let tokens = vec![
            TokenInfo {
                token_id: "t1".into(),
                outcome: "A".into(),
                price: dec!(0.95),
            },
            TokenInfo {
                token_id: "t2".into(),
                outcome: "B".into(),
                price: dec!(0.03),
            },
            TokenInfo {
                token_id: "t3".into(),
                outcome: "C".into(),
                price: dec!(0.02),
            },
        ];
        let market = make_market("c1", Some(end), tokens, vec![], false);
        let books = make_books_with_ask("t1", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn evaluate_respects_max_bet() {
        let config = test_config();
        let tracker = DecayTracker::new();
        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(100),
            is_sports: false,
            score: 0.1,
        };

        let intent = evaluate_decay_buy(&candidate, &config, &tracker, dec!(1000)).unwrap();
        // $15 / $0.95 = 15.78 shares (rounded down)
        assert!(intent.size <= dec!(15.79));
        assert!(intent.size >= dec!(15.0));
        assert_eq!(intent.order_type, OrderType::FOK);
    }

    #[test]
    fn evaluate_skips_duplicate_market() {
        let config = test_config();
        let mut tracker = DecayTracker::new();
        tracker.record_fill("c1", "tok_yes", "Yes", dec!(2), dec!(0.95), false, dec!(0));

        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(100),
            is_sports: false,
            score: 0.1,
        };

        assert!(evaluate_decay_buy(&candidate, &config, &tracker, dec!(1000)).is_none());
    }

    #[test]
    fn evaluate_respects_nav_cap() {
        let mut config = test_config();
        config.nav_usdc = 100.0;
        config.decay_nav_fraction = 0.50; // $50 budget
        config.decay_max_bet_usdc = 2.0;

        let mut tracker = DecayTracker::new();
        // Deploy $50 already
        tracker.record_fill(
            "c_old",
            "t_old",
            "Yes",
            dec!(52),
            dec!(0.96),
            false,
            dec!(0),
        );

        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(100),
            is_sports: false,
            score: 0.1,
        };

        assert!(evaluate_decay_buy(&candidate, &config, &tracker, dec!(1000)).is_none());
    }

    #[test]
    fn evaluate_caps_size_to_available_liquidity() {
        let config = test_config();
        let tracker = DecayTracker::new();
        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(5), // only 5 shares at best ask
            is_sports: false,
            score: 0.1,
        };

        let intent = evaluate_decay_buy(&candidate, &config, &tracker, dec!(1000)).unwrap();
        assert_eq!(intent.size, dec!(5)); // capped to available
    }

    #[test]
    fn evaluate_skips_when_available_below_min_order() {
        let config = test_config();
        let tracker = DecayTracker::new();
        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(15.0),
            available_size: dec!(3), // only 3 shares, min is 15
            is_sports: false,
            score: 0.1,
        };

        assert!(evaluate_decay_buy(&candidate, &config, &tracker, dec!(1000)).is_none());
    }

    #[test]
    fn tracker_cleanup_removes_resolved() {
        let mut tracker = DecayTracker::new();
        tracker.record_fill("c1", "t1", "Yes", dec!(2), dec!(0.95), false, dec!(0));
        tracker.record_fill("c2", "t2", "Yes", dec!(2), dec!(0.95), false, dec!(0));

        // Only c1 is still active
        let markets = vec![make_market(
            "c1",
            None,
            make_tokens("0.95", "0.05"),
            vec![],
            false,
        )];
        tracker.cleanup_resolved(&markets);
        assert_eq!(tracker.position_count(), 1);
        assert!(tracker.positions.contains_key("c1"));
    }

    #[test]
    fn candidates_sorted_by_score_descending_not_price() {
        // When price and time differ, score determines order (edge * turnover)
        // Higher edge (lower price) can beat higher certainty (higher price)
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);

        let m1 = make_market(
            "c1",
            Some(end),
            vec![
                TokenInfo {
                    token_id: "t1".into(),
                    outcome: "Yes".into(),
                    price: dec!(0.93),
                },
                TokenInfo {
                    token_id: "t1n".into(),
                    outcome: "No".into(),
                    price: dec!(0.07),
                },
            ],
            vec![],
            false,
        );
        let m2 = make_market(
            "c2",
            Some(end),
            vec![
                TokenInfo {
                    token_id: "t2".into(),
                    outcome: "Yes".into(),
                    price: dec!(0.97),
                },
                TokenInfo {
                    token_id: "t2n".into(),
                    outcome: "No".into(),
                    price: dec!(0.03),
                },
            ],
            vec![],
            false,
        );

        let books = BookStore::new();
        {
            use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
            use polymarket_client_sdk::types::{B256, U256};
            for (tid, ask) in [("t1", dec!(0.93)), ("t2", dec!(0.97))] {
                let level = OrderBookLevel::builder().price(ask).size(dec!(100)).build();
                let update = BookUpdate::builder()
                    .asset_id(U256::ZERO)
                    .market(B256::ZERO)
                    .timestamp(1000)
                    .bids(vec![])
                    .asks(vec![level])
                    .hash("test".into())
                    .build();
                books.apply(tid, &update);
            }
        }

        let config = test_config();
        let candidates = scan_candidates(&[m1, m2], &books, &config, now);
        assert_eq!(candidates.len(), 2);
        // 0.93 has larger edge (0.07 vs 0.03) with same turnover
        // Even with price bonus for 0.97, the edge difference dominates
        assert!(
            candidates[0].score >= candidates[1].score,
            "should be sorted by score"
        );
    }

    #[test]
    fn scan_sports_market_within_sports_window() {
        let now = Utc::now();
        // 3 minutes = within 5 min sports window
        let end = now + ChronoDuration::minutes(3);
        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.95", "0.05"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(
            candidates.len(),
            1,
            "sports market within 5min window should be found"
        );
    }

    #[test]
    fn scan_sports_market_outside_sports_window_skipped() {
        let now = Utc::now();
        // 10 minutes = outside 5 min sports window, but within 24h normal window
        let end = now + ChronoDuration::minutes(10);
        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.95", "0.05"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(
            candidates.is_empty(),
            "sports market outside 5min window should be skipped"
        );
    }

    #[test]
    fn scan_non_sports_within_24h_window() {
        let now = Utc::now();
        // 12 hours = within 24h normal window
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(
            candidates.len(),
            1,
            "non-sports market within 24h window should be found"
        );
    }

    #[test]
    fn scan_non_sports_outside_24h_window_skipped() {
        let now = Utc::now();
        // 48 hours = outside 24h normal window
        let end = now + ChronoDuration::hours(48);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(
            candidates.is_empty(),
            "non-sports market outside 24h window should be skipped"
        );
    }

    #[test]
    fn is_sports_market_detects_various_tags() {
        let sports_tags = vec![
            vec!["Sports"],
            vec!["Football"],
            vec!["Basketball"],
            vec!["NFL"],
            vec!["NBA"],
            vec!["Soccer"],
            vec!["Baseball"],
            vec!["MLB"],
            vec!["Hockey"],
            vec!["NHL"],
            vec!["Tennis"],
            vec!["Golf"],
            vec!["Boxing"],
            vec!["MMA"],
            vec!["NCAA"],
        ];

        for tags in sports_tags {
            let market = TradableMarket {
                condition_id: "c1".into(),
                question: "Test?".into(),
                tokens: vec![],
                neg_risk: false,
                neg_risk_market_id: None,
                min_tick_size: dec!(0.01),
                min_order_size: dec!(1.0),
                maker_fee_bps: dec!(0),
                rewards_active: false,
                rewards_max_spread: None,
                rewards_min_size: None,
                volume_24h: 0.0,
                tags: tags.iter().map(|s| s.to_string()).collect(),
                end_date: None,
            };
            assert!(
                is_sports_market(&market),
                "should detect {:?} as sports",
                tags
            );
        }
    }

    #[test]
    fn evaluate_uses_sports_max_bet_for_sports_candidates() {
        let config = test_config();
        let tracker = DecayTracker::new();

        // Sports candidate should use decay_sports_max_bet_usdc ($5)
        let sports_candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 0.05, // 3 mins
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(100),
            is_sports: true,
            score: 0.5,
        };

        let intent = evaluate_decay_buy(&sports_candidate, &config, &tracker, dec!(1000)).unwrap();
        // $5 / $0.95 = 5.26 shares (rounded down)
        assert!(
            intent.size <= dec!(5.27),
            "sports bet should be capped at $5"
        );
        assert!(
            intent.size >= dec!(5.0),
            "sports bet should be at least $5 worth"
        );

        // Non-sports candidate should use decay_max_bet_usdc ($15)
        let normal_candidate = DecayCandidate {
            condition_id: "c2".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
            available_size: dec!(100),
            is_sports: false,
            score: 0.1,
        };

        let intent2 = evaluate_decay_buy(&normal_candidate, &config, &tracker, dec!(1000)).unwrap();
        // $15 / $0.95 = 15.78 shares (rounded down)
        assert!(
            intent2.size <= dec!(15.79),
            "normal bet should be capped at $15"
        );
        assert!(
            intent2.size >= dec!(15.0),
            "normal bet should be at least $15 worth"
        );
    }

    #[test]
    fn scan_sports_requires_higher_min_price() {
        let now = Utc::now();
        let end = now + ChronoDuration::minutes(3); // within sports window

        // Price at 0.85 - should qualify for normal (min 0.80) but not sports (min 0.90)
        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.85", "0.15"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.85));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(
            candidates.is_empty(),
            "sports market at 0.85 should be skipped (min is 0.90)"
        );
    }

    #[test]
    fn scan_sports_at_90_price_qualifies() {
        let now = Utc::now();
        let end = now + ChronoDuration::minutes(3); // within sports window

        // Price at 0.90 - should qualify for sports (min 0.90)
        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.90", "0.10"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.90));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(candidates.len(), 1, "sports market at 0.90 should qualify");
    }

    #[test]
    fn scan_sports_in_progress_game_found() {
        let now = Utc::now();
        // Game started 1 hour ago (negative secs_remaining)
        let end = now - ChronoDuration::hours(1);

        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.95", "0.05"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(
            candidates.len(),
            1,
            "sports game in progress should be found"
        );
        assert!(candidates[0].score > 0.0, "candidate should have a score");
    }

    #[test]
    fn scan_sports_in_progress_2x_bonus_applied() {
        // Verify that in-progress games get the 2x bonus
        // We test this by comparing same time-to-resolution with/without in-progress flag
        let now = Utc::now();

        // Game in progress: started 2.5 hours ago, ~1 hour remaining
        let in_progress_end = now - ChronoDuration::seconds(9000); // 2.5 hours ago
        let m_in_progress = make_market(
            "c1",
            Some(in_progress_end),
            make_tokens("0.95", "0.05"),
            vec!["Sports".to_string()],
            false,
        );

        let books = BookStore::new();
        {
            use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
            use polymarket_client_sdk::types::{B256, U256};
            let level = OrderBookLevel::builder()
                .price(dec!(0.95))
                .size(dec!(100))
                .build();
            let update = BookUpdate::builder()
                .asset_id(U256::ZERO)
                .market(B256::ZERO)
                .timestamp(1000)
                .bids(vec![])
                .asks(vec![level])
                .hash("test".into())
                .build();
            books.apply("tok_yes", &update);
        }

        let config = test_config();

        let candidates = scan_candidates(&[m_in_progress], &books, &config, now);
        assert_eq!(candidates.len(), 1);

        // Manually calculate expected score to verify 2x bonus is applied
        // price = 0.95, edge = 0.05, bonus = 1.2 (>= 0.95)
        // secs_to_resolution = 12600 - 9000 = 3600 (1 hour)
        // abs_hours = 1.0, turnover = 1 / 1.1 = 0.91
        // base_score = 0.05 * 0.91 * 1.2 = 0.0545
        // with 2x bonus = 0.109
        let score = candidates[0].score;
        assert!(
            score > 0.1,
            "in-progress score {} should include 2x bonus",
            score
        );
        assert!(
            score < 0.15,
            "in-progress score {} should be reasonable",
            score
        );
    }

    #[test]
    fn scan_sports_game_too_old_skipped() {
        let now = Utc::now();
        // Game started 5 hours ago (beyond MAX_GAME_DURATION_SECS)
        let end = now - ChronoDuration::hours(5);

        let market = make_market(
            "c1",
            Some(end),
            make_tokens("0.95", "0.05"),
            vec!["Sports".to_string()],
            false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(
            candidates.is_empty(),
            "sports game too old should be skipped"
        );
    }

    #[test]
    fn score_opportunity_favors_shorter_resolution() {
        let now = Utc::now();

        // Two markets with same price but different resolution times
        let end_soon = now + ChronoDuration::hours(1);
        let end_later = now + ChronoDuration::hours(12);

        let m1 = make_market(
            "c1",
            Some(end_soon),
            make_tokens("0.95", "0.05"),
            vec![],
            false,
        );
        let m2 = make_market(
            "c2",
            Some(end_later),
            make_tokens("0.95", "0.05"),
            vec![],
            false,
        );

        let books = BookStore::new();
        {
            use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
            use polymarket_client_sdk::types::{B256, U256};
            for (tid, ask) in [("tok_yes", dec!(0.95))] {
                let level = OrderBookLevel::builder().price(ask).size(dec!(100)).build();
                let update = BookUpdate::builder()
                    .asset_id(U256::ZERO)
                    .market(B256::ZERO)
                    .timestamp(1000)
                    .bids(vec![])
                    .asks(vec![level])
                    .hash("test".into())
                    .build();
                books.apply(tid, &update);
            }
        }

        let config = test_config();
        let candidates = scan_candidates(&[m1, m2], &books, &config, now);

        assert_eq!(candidates.len(), 2);
        // Shorter resolution should be ranked first (higher score)
        assert!(
            candidates[0].score > candidates[1].score,
            "shorter resolution should have higher score"
        );
    }

    #[test]
    fn candidates_sorted_by_score_descending() {
        let now = Utc::now();
        // Different prices, different times - scoring determines order
        let end1 = now + ChronoDuration::hours(1); // 0.93 @ 1hr
        let end2 = now + ChronoDuration::hours(12); // 0.97 @ 12hr

        let m1 = make_market(
            "c1",
            Some(end1),
            vec![
                TokenInfo {
                    token_id: "t1".into(),
                    outcome: "Yes".into(),
                    price: dec!(0.93),
                },
                TokenInfo {
                    token_id: "t1n".into(),
                    outcome: "No".into(),
                    price: dec!(0.07),
                },
            ],
            vec![],
            false,
        );
        let m2 = make_market(
            "c2",
            Some(end2),
            vec![
                TokenInfo {
                    token_id: "t2".into(),
                    outcome: "Yes".into(),
                    price: dec!(0.97),
                },
                TokenInfo {
                    token_id: "t2n".into(),
                    outcome: "No".into(),
                    price: dec!(0.03),
                },
            ],
            vec![],
            false,
        );

        let books = BookStore::new();
        {
            use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
            use polymarket_client_sdk::types::{B256, U256};
            for (tid, ask) in [("t1", dec!(0.93)), ("t2", dec!(0.97))] {
                let level = OrderBookLevel::builder().price(ask).size(dec!(100)).build();
                let update = BookUpdate::builder()
                    .asset_id(U256::ZERO)
                    .market(B256::ZERO)
                    .timestamp(1000)
                    .bids(vec![])
                    .asks(vec![level])
                    .hash("test".into())
                    .build();
                books.apply(tid, &update);
            }
        }

        let config = test_config();
        let candidates = scan_candidates(&[m1, m2], &books, &config, now);

        assert_eq!(candidates.len(), 2);
        // Should be sorted by score, not price
        assert!(
            candidates[0].score >= candidates[1].score,
            "candidates should be sorted by score descending"
        );
    }
}
