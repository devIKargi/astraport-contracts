//! Slippage protection for the AstraPort Trading Engine.
//!
//! Slippage is defined as the percentage deviation between a user's limit
//! price and the actual fill price.  If the deviation exceeds the
//! configured `max_slippage_bps`, the execution is rejected.

use soroban_sdk::{Env, Symbol};

use crate::orderbook::load_book;
use crate::types::*;

/// Set the slippage configuration for a trading pair.
pub fn set_slippage_config(env: &Env, pair_id: &Symbol, config: &SlippageConfig) {
    env.storage()
        .persistent()
        .set(&TradeDataKey::SlippageConfig(pair_id.clone()), config);
}

/// Get the slippage configuration for a pair, falling back to defaults.
pub fn get_slippage_config(env: &Env, pair_id: &Symbol) -> SlippageConfig {
    env.storage()
        .persistent()
        .get(&TradeDataKey::SlippageConfig(pair_id.clone()))
        .unwrap_or_default()
}

/// Check whether a proposed fill violates the slippage tolerance.
///
/// # Arguments
/// * `limit_price` — the price the user specified in their order.
/// * `fill_price` — the price at which the matching engine would fill.
/// * `side` — whether the user is buying or selling.
/// * `config` — the pair's slippage configuration.
///
/// Returns `Ok(())` if within tolerance, `Err(TradeError::SlippageExceeded)`
/// otherwise.
pub fn check_slippage(
    _env: &Env,
    limit_price: i128,
    fill_price: i128,
    side: OrderSide,
    config: &SlippageConfig,
) -> Result<(), TradeError> {
    if !config.enabled || config.max_slippage_bps <= 0 {
        return Ok(());
    }

    if limit_price <= 0 || fill_price <= 0 {
        return Err(TradeError::SlippageExceeded);
    }

    // For a BUY order, slippage occurs when the fill price is HIGHER than the
    // limit price (you pay more than expected).  For a SELL order, slippage
    // occurs when the fill price is LOWER than the limit price.
    let slippage_bps = match side {
        OrderSide::Buy => {
            if fill_price <= limit_price {
                // Getting a better price — no slippage concern.
                return Ok(());
            }
            // `bps = (fill - limit) * 10000 / limit`
            ((fill_price - limit_price) * BPS_DENOM) / limit_price
        }
        OrderSide::Sell => {
            if fill_price >= limit_price {
                return Ok(());
            }
            ((limit_price - fill_price) * BPS_DENOM) / limit_price
        }
    };

    if slippage_bps > config.max_slippage_bps {
        return Err(TradeError::SlippageExceeded);
    }

    Ok(())
}

/// Validate slippage against the current best available price in the book.
///
/// This is used for pre-flight checks before submitting an atomic batch.
/// It looks at the best ask (for buys) or best bid (for sells) and verifies
/// that the user's limit price is within the slippage tolerance.
pub fn validate_slippage_pre_trade(
    env: &Env,
    pair_id: &Symbol,
    side: OrderSide,
    limit_price: i128,
    _amount: i128,
    config_override: Option<&SlippageConfig>,
) -> Result<(), TradeError> {
    let config = config_override
        .cloned()
        .unwrap_or_else(|| get_slippage_config(env, pair_id));
    if !config.enabled || config.max_slippage_bps <= 0 {
        return Ok(());
    }

    let book = load_book(env, pair_id);

    // For a buy, the worst-case fill price is the best (lowest) ask.
    // For a sell, the worst-case fill price is the best (highest) bid.
    let worst_price = match side {
        OrderSide::Buy => {
            if book.asks.is_empty() {
                return Ok(()); // No asks — no slippage risk from existing book.
            }
            book.asks.get(0).unwrap().price
        }
        OrderSide::Sell => {
            if book.bids.is_empty() {
                return Ok(());
            }
            book.bids.get(0).unwrap().price
        }
    };

    check_slippage(env, limit_price, worst_price, side, &config)
}

/// Compute the minimum acceptable fill price for a buy order given the limit
/// price and max slippage.  Returns 0 if slippage is disabled.
pub fn min_acceptable_buy_price(limit_price: i128, config: &SlippageConfig) -> i128 {
    if !config.enabled || config.max_slippage_bps <= 0 || limit_price <= 0 {
        return 0; // No constraint.
    }
    // Allow price to go up by at most `max_slippage_bps`.
    limit_price + (limit_price * config.max_slippage_bps / BPS_DENOM)
}

/// Compute the minimum acceptable fill price for a sell order given the limit
/// price and max slippage.  Returns 0 if slippage is disabled.
pub fn min_acceptable_sell_price(limit_price: i128, config: &SlippageConfig) -> i128 {
    if !config.enabled || config.max_slippage_bps <= 0 || limit_price <= 0 {
        return 0; // No constraint — any price is fine.
    }
    // Allow price to drop by at most `max_slippage_bps`.
    let min_price = limit_price - (limit_price * config.max_slippage_bps / BPS_DENOM);
    if min_price < 1 {
        1
    } else {
        min_price
    }
}
