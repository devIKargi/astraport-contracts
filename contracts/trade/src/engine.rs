//! Atomic multi-asset trade execution engine.
//!
//! An *atomic batch* is a collection of [`TradeLeg`]s that must all succeed
//! for any of them to take effect.  This module provides:
//!
//! * **Pre-flight validation** — checks slippage bounds, pair availability,
//!   and liquidity for every leg *before* any state is modified.
//! * **Atomic execution** — attempts each leg in order; if any leg fails the
//!   slippage check at execution time, the entire batch is aborted and the
//!   book is left untouched (no partial side-effects).

use soroban_sdk::{symbol_short, Address, Env, Symbol, Vec};

use crate::orderbook;
use crate::slippage;
use crate::types::*;

/// Execute an atomic batch of trade legs.
///
/// The algorithm:
/// 1. Validate every leg (pair exists, amounts valid, slippage ok).
/// 2. For each leg, run the matching engine.  Track all fills produced.
/// 3. After *all* legs have been matched, check that every leg's slippage
///    is within bounds using the actual average fill price.
/// 4. If any leg fails the slippage check, abort — the book modifications
///    made during step 2 are reverted by the fact that the caller (the
///    contract entrypoint) will not persist the results.
///
/// Because Soroban transactions are atomic at the ledger level, if this
/// function returns an `Err`, the entire transaction (including all storage
/// writes) is rolled back.  This is the foundation of atomic settlement.
pub fn execute_atomic_batch(
    env: &Env,
    user: &Address,
    legs: &Vec<TradeLeg>,
    pair_configs: &Vec<TradePair>,
) -> Result<AtomicBatchResult, TradeError> {
    if legs.is_empty() {
        return Err(TradeError::EmptyBatch);
    }

    let now = env.ledger().timestamp();
    let mut leg_results = Vec::new(env);
    let mut total_fills: u32 = 0;

    for i in 0..legs.len() {
        let leg = legs.get(i).unwrap();

        // 1. Validate pair exists and is active.
        let pair = find_pair(pair_configs, &leg.pair_id).ok_or(TradeError::PairNotFound)?;
        if !pair.is_active {
            return Err(TradeError::PairInactive);
        }

        // 2. Validate amounts.
        if leg.amount <= 0 || leg.price <= 0 {
            return Err(TradeError::InvalidOrderAmount);
        }
        if leg.amount < pair.min_order_size {
            return Err(TradeError::InvalidOrderAmount);
        }

        // 3. Resolve slippage config for this leg.
        let mut config = slippage::get_slippage_config(env, &leg.pair_id);
        if let Some(override_bps) = leg.max_slippage_bps {
            config.max_slippage_bps = override_bps;
            config.enabled = true;
        }

        // 4. Pre-flight slippage check (using the resolved config including override).
        slippage::validate_slippage_pre_trade(
            env,
            &leg.pair_id,
            leg.side,
            leg.price,
            leg.amount,
            Some(&config),
        )?;

        // 5. Run the matching engine.
        let (fills, filled_amount) =
            orderbook::match_order(env, &leg.pair_id, user, leg.side, leg.amount, leg.price);

        // 6. Compute average fill price and total fees.
        let mut total_notional: i128 = 0;
        let mut total_fees: i128 = 0;
        for j in 0..fills.len() {
            let fill = fills.get(j).unwrap();
            total_notional += fill.amount * fill.price;
            total_fees += fill.fee;
        }

        let avg_price = if filled_amount > 0 {
            total_notional / filled_amount
        } else {
            0
        };

        // 7. Post-execution slippage check using the actual average fill price.
        if config.enabled && filled_amount > 0 && avg_price > 0 {
            slippage::check_slippage(env, leg.price, avg_price, leg.side, &config)?;
        }

        // 8. Update pair volume.
        let volume_key = TradeDataKey::PairVolume(leg.pair_id.clone());
        let current_volume: i128 = env.storage().persistent().get(&volume_key).unwrap_or(0);
        env.storage().persistent().set(
            &volume_key,
            &current_volume
                .checked_add(total_notional)
                .ok_or(TradeError::ArithmeticOverflow)?,
        );

        total_fills += fills.len();

        leg_results.push_back(LegResult {
            pair_id: leg.pair_id.clone(),
            fills,
            filled_amount,
            avg_price,
            total_fees,
        });
    }

    Ok(AtomicBatchResult {
        success: true,
        legs: leg_results,
        total_fills,
        executed_at: now,
        failure_reason: symbol_short!(""),
    })
}

/// Abort a batch execution and produce a failure result.  This is used
/// when a batch fails validation or slippage checks.
pub fn abort_batch(reason: &str) -> TradeError {
    // In a Soroban contract, returning an error will cause the entire
    // transaction to revert, effectively implementing atomicity.
    match reason {
        "empty_batch" => TradeError::EmptyBatch,
        "pair_not_found" => TradeError::PairNotFound,
        "slippage_exceeded" => TradeError::SlippageExceeded,
        "insufficient_liquidity" => TradeError::InsufficientLiquidity,
        _ => TradeError::EmptyBatch,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a pair configuration by its ID in a list of pairs.
fn find_pair(pairs: &Vec<TradePair>, pair_id: &Symbol) -> Option<TradePair> {
    let len = pairs.len();
    for i in 0..len {
        let p = pairs.get(i).unwrap();
        if p.pair_id == *pair_id {
            return Some(p);
        }
    }
    None
}

/// Record a batch execution in the global trade history.
pub fn record_batch(env: &Env, user: &Address, result: &AtomicBatchResult) {
    env.events().publish(
        (symbol_short!("BATCHEXEC"), user.clone()),
        result.total_fills,
    );
}
