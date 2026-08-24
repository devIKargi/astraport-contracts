//! Emergency unstaking with penalty system.
//!
//! This module implements early withdrawal from a locked stake position before
//! the scheduled unlock date. Penalties are assessed at the time of withdrawal
//! and decay over time as the unlock approaches, balancing user flexibility
//! against protocol incentives.
//!
//! # Penalty Decay
//!
//! Two decay functions are supported:
//!
//! - **Linear**: The penalty decreases uniformly from `penalty_start_bps` at
//!   lock-start to `penalty_end_bps` at the unlock date.
//!   `penalty(t) = start + (end - start) * elapsed / total_lock_seconds`
//!
//! - **Exponential**: The penalty decreases rapidly early in the lock period and
//!   slowly near the end, modeled as
//!   `penalty(t) = start * (end / start)^(elapsed / total_lock_seconds)`.
//!
//! All penalty rates are expressed in basis points (1 bp = 0.01%). The range
//! [0, 10_000] maps to [0%, 100%].
//!
//! # Storage Layout
//!
//! Emergency unstake data uses [`EmergencyDataKey`] variants:
//! - `Config` — a single [`EmergencyUnstakeConfig`] per contract.
//! - `CooldownEnd(Address)` — the ledger timestamp after which a staker may
//!   emergency-unstake again (0 if no cooldown is active).
//! - `History(Address)` — append-only `Vec<EmergencyUnstakeRecord>` per staker.

use soroban_sdk::{contracttype, symbol_short, Address, Env, Vec};

use crate::fixed_point::{self as fp, MathError, ONE, SCALE};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum allowed basis-point value (= 100%).
pub const MAX_BPS: i128 = 10_000;

/// The fixed-point representation of 10_000 (basis-point denominator).
const BPS_SCALE: i128 = 10_000;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Selects how the penalty rate decays toward the unlock date.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenaltyDecayFunction {
    /// Penalty decreases linearly from start to end over the full lock period.
    Linear,
    /// Penalty decreases exponentially: fast early, slow near the end.
    Exponential,
    /// A custom decay that always returns `penalty_start_bps` unchanged.
    /// Useful for fixed-rate penalty schemes; callers may extend logic here.
    Custom,
}

/// Contract-wide configuration for the emergency-unstake system.
///
/// Stored under [`EmergencyDataKey::Config`]. All basis-point fields are in
/// the range [0, 10_000], where 10_000 = 100%.
#[contracttype]
#[derive(Debug, Clone)]
pub struct EmergencyUnstakeConfig {
    /// Penalty rate at the *start* of the lock period, in basis points.
    /// Assessed when `elapsed == 0`.
    pub penalty_start_bps: i128,
    /// Penalty rate at the *end* of the lock period (at unlock), in basis
    /// points. Assessed when `elapsed == total_lock_seconds`.
    pub penalty_end_bps: i128,
    /// How the penalty interpolates between start and end.
    pub decay_function: PenaltyDecayFunction,
    /// Duration (in seconds) a staker must wait after an emergency unstake
    /// before they are permitted to do another one.
    pub cooldown_seconds: u64,
    /// Treasury address that receives all collected penalties.
    pub treasury: Address,
    /// Whether emergency unstaking is currently enabled on this contract.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// An immutable record of a single emergency unstake event.
///
/// Appended to the staker's history log under [`EmergencyDataKey::History`].
#[contracttype]
#[derive(Debug, Clone)]
pub struct EmergencyUnstakeRecord {
    /// The staker who performed the emergency unstake.
    pub staker: Address,
    /// Ledger timestamp (seconds) at which the operation occurred.
    pub timestamp: u64,
    /// Principal amount the staker attempted to withdraw.
    pub amount_requested: i128,
    /// Penalty amount deducted from `amount_requested`.
    pub penalty_amount: i128,
    /// Final amount returned to the staker (`amount_requested - penalty_amount`).
    pub amount_returned: i128,
    /// Penalty rate actually applied, in basis points.
    pub penalty_bps_applied: i128,
    /// Ledger timestamp when the staker's lock period was originally set to end.
    pub original_unlock_ts: u64,
    /// Ledger timestamp when the lock period originally started (= when the
    /// stake was first locked).
    pub lock_start_ts: u64,
    /// Whether this was a partial (true) or full (false) emergency unstake.
    pub is_partial: bool,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// Storage keys for the emergency-unstake subsystem.
#[contracttype]
#[derive(Debug, Clone)]
pub enum EmergencyDataKey {
    /// The single contract-wide [`EmergencyUnstakeConfig`].
    Config,
    /// The ledger timestamp after which `Address` may emergency-unstake again.
    /// A value of 0 means no cooldown is active.
    CooldownEnd(Address),
    /// Append-only `Vec<EmergencyUnstakeRecord>` log for an address.
    History(Address),
}

// ---------------------------------------------------------------------------
// Penalty calculator
// ---------------------------------------------------------------------------

/// Computes the penalty basis points applicable to an emergency unstake.
///
/// # Arguments
///
/// * `config` — the active [`EmergencyUnstakeConfig`].
/// * `elapsed_seconds` — seconds elapsed since the lock was opened.
/// * `total_lock_seconds` — full intended lock duration in seconds.
///
/// Returns basis points in `[penalty_end_bps, penalty_start_bps]`, or an error
/// if the fixed-point arithmetic overflows.
pub struct PenaltyCalculator;

impl PenaltyCalculator {
    /// Compute the penalty in basis points for an emergency unstake.
    ///
    /// The result is clamped to `[0, MAX_BPS]`.
    pub fn compute_penalty_bps(
        config: &EmergencyUnstakeConfig,
        elapsed_seconds: u64,
        total_lock_seconds: u64,
    ) -> Result<i128, MathError> {
        // Edge cases: if the lock has not started or there is no lock duration,
        // apply the start penalty.
        if total_lock_seconds == 0 || elapsed_seconds == 0 {
            return Ok(config.penalty_start_bps.min(MAX_BPS));
        }
        // If elapsed >= total, the lock has matured — apply end penalty.
        if elapsed_seconds >= total_lock_seconds {
            return Ok(config.penalty_end_bps.clamp(0, MAX_BPS));
        }

        let start = config.penalty_start_bps;
        let end = config.penalty_end_bps;

        let bps = match config.decay_function {
            PenaltyDecayFunction::Linear => {
                Self::linear_decay(start, end, elapsed_seconds, total_lock_seconds)?
            }
            PenaltyDecayFunction::Exponential => {
                Self::exponential_decay(start, end, elapsed_seconds, total_lock_seconds)?
            }
            PenaltyDecayFunction::Custom => {
                // Custom: fixed at start penalty regardless of elapsed time.
                start
            }
        };

        Ok(bps.clamp(0, MAX_BPS))
    }

    /// Linear interpolation between `start` and `end`.
    ///
    /// `bps = start + (end - start) * elapsed / total`
    fn linear_decay(start: i128, end: i128, elapsed: u64, total: u64) -> Result<i128, MathError> {
        // progress = elapsed / total  (fixed-point)
        let progress = fp::div(elapsed as i128, total as i128)?;
        let delta = end - start; // can be negative if end < start (normal case)
        let change = fp::mul(delta, progress)?;
        start.checked_add(change).ok_or(MathError::Overflow)
    }

    /// Exponential decay: `bps = start * (end / start)^(elapsed / total)`.
    ///
    /// Computed as `exp(ln(end/start) * (elapsed/total)) * start`.
    ///
    /// If `start == 0` or `end == 0` the formula degenerates; we fall back to
    /// linear in those cases to avoid a `ln(0)` domain error.
    fn exponential_decay(
        start: i128,
        end: i128,
        elapsed: u64,
        total: u64,
    ) -> Result<i128, MathError> {
        if start <= 0 || end <= 0 {
            // Degenerate: use linear when one bound is zero.
            return Self::linear_decay(start, end, elapsed, total);
        }

        // t = elapsed / total  (fixed-point fraction in [0, 1))
        let t = fp::div(elapsed as i128, total as i128)?;

        // ratio = end / start  (fixed-point)
        // Use plain integer multiplication rather than `fp::mul` here. The fixed-
        // point helper divides by SCALE inside the call, which truncates the
        // intermediate to 0 for typical BPS values (e.g. start = 4000) and
        // produces a spurious DivideByZero in the next division.
        let start_fp = start * (SCALE / BPS_SCALE);
        let end_fp = end * (SCALE / BPS_SCALE);
        let ratio = fp::div(end_fp, start_fp)?;

        // ln(ratio) and exponent = ln(ratio) * t
        // Both start and end are in [0, MAX_BPS]; ratio is in (0, ∞).
        // If ratio < ONE (normal: end < start) ln is negative, but exp requires
        // non-negative input. We handle this by working with 1/ratio and
        // negating afterward.
        let bps_fp = if ratio >= ONE {
            // end >= start (unusual, penalty growing over time — still valid)
            let ln_ratio = crate::apy::ln(ratio)?;
            let exponent = fp::mul(ln_ratio, t)?;
            let factor = fp::exp(exponent)?;
            fp::mul(start_fp, factor)?
        } else {
            // end < start (normal decay)
            let inv_ratio = fp::div(ONE, ratio)?;
            let ln_inv = crate::apy::ln(inv_ratio)?;
            let exponent = fp::mul(ln_inv, t)?;
            let factor = fp::exp(exponent)?;
            // result = start / factor  (since start * (1/ratio)^t = start / ratio^t)
            fp::div(start_fp, factor)?
        };

        // Convert back from fixed-point fraction to basis points.
        fp::mul(bps_fp, BPS_SCALE * ONE / SCALE)
    }

    /// Apply a penalty in basis points to an amount.
    ///
    /// Returns `(penalty_amount, net_amount)`.
    pub fn apply_penalty(amount: i128, penalty_bps: i128) -> Result<(i128, i128), MathError> {
        if penalty_bps == 0 {
            return Ok((0, amount));
        }
        if penalty_bps >= MAX_BPS {
            return Ok((amount, 0));
        }
        // penalty = amount * penalty_bps / 10_000
        let penalty = fp::mul_div(amount, penalty_bps, BPS_SCALE)?;
        let net = amount.checked_sub(penalty).ok_or(MathError::Overflow)?;
        Ok((penalty, net))
    }
}

// ---------------------------------------------------------------------------
// State machine / execution
// ---------------------------------------------------------------------------

/// Executes the full emergency-unstake flow:
/// 1. Validate the request (enabled, cooldown clear, sufficient balance).
/// 2. Compute the penalty.
/// 3. Distribute the penalty to the treasury.
/// 4. Record the event in the staker's history.
/// 5. Activate the post-unstake cooldown.
///
/// Returns the [`EmergencyUnstakeRecord`] describing the operation.
///
/// # Errors
///
/// Panics (via `expect` / `assert`) on invalid inputs; the caller is
/// responsible for forwarding these as contract errors through the standard
/// `Error` enum in `lib.rs`.
pub struct EmergencyUnstakeExecutor;

impl EmergencyUnstakeExecutor {
    /// Validate pre-conditions and execute the emergency unstake.
    ///
    /// * `env` — the Soroban execution environment.
    /// * `staker` — the staker requesting early withdrawal.
    /// * `amount` — how many base units to withdraw (may be partial).
    /// * `current_balance` — the staker's current staked balance.
    /// * `lock_start_ts` — when the current lock period started (ledger time).
    /// * `unlock_ts` — when the lock would normally expire (ledger time).
    ///
    /// On success, returns the [`EmergencyUnstakeRecord`] (without persisting
    /// the balance change — the caller's stake/unstake handler is responsible
    /// for updating the balance in its own storage key, since the balance key
    /// is owned by `lib.rs`).
    pub fn execute(
        env: &Env,
        staker: &Address,
        amount: i128,
        current_balance: i128,
        lock_start_ts: u64,
        unlock_ts: u64,
    ) -> Result<EmergencyUnstakeRecord, crate::Error> {
        let now = env.ledger().timestamp();

        // --- load config --------------------------------------------------
        let config: EmergencyUnstakeConfig =
            match env.storage().persistent().get(&EmergencyDataKey::Config) {
                Some(c) => c,
                None => return Err(crate::Error::EmergencyConfigNotInitialized),
            };

        if !config.enabled {
            return Err(crate::Error::EmergencyUnstakeDisabled);
        }
        if amount <= 0 {
            return Err(crate::Error::InvalidEmergencyUnstakeAmount);
        }
        if amount > current_balance {
            return Err(crate::Error::InsufficientBalance);
        }

        // --- cooldown check -----------------------------------------------
        let cooldown_end: u64 = env
            .storage()
            .persistent()
            .get(&EmergencyDataKey::CooldownEnd(staker.clone()))
            .unwrap_or(0);
        if now < cooldown_end {
            return Err(crate::Error::CooldownActive);
        }

        // --- compute penalty ----------------------------------------------
        let total_lock_seconds = unlock_ts.saturating_sub(lock_start_ts);
        let elapsed = now.saturating_sub(lock_start_ts);

        let penalty_bps =
            PenaltyCalculator::compute_penalty_bps(&config, elapsed, total_lock_seconds)
                .map_err(|_| crate::Error::InvalidEmergencyUnstakeAmount)?;

        let (penalty_amount, amount_returned) =
            PenaltyCalculator::apply_penalty(amount, penalty_bps)
                .expect("PenaltyApplicationFailed");

        // --- distribute penalty to treasury (event) -----------------------
        // In a real token contract this would be a token transfer. Here we
        // emit an event so off-chain observers and the treasury can act.
        if penalty_amount > 0 {
            env.events().publish(
                (symbol_short!("PENALTY"), staker.clone()),
                PenaltyDistributionEvent {
                    staker: staker.clone(),
                    treasury: config.treasury.clone(),
                    penalty_amount,
                    timestamp: now,
                },
            );
        }

        // --- build record -------------------------------------------------
        let is_partial = amount < current_balance;
        let record = EmergencyUnstakeRecord {
            staker: staker.clone(),
            timestamp: now,
            amount_requested: amount,
            penalty_amount,
            amount_returned,
            penalty_bps_applied: penalty_bps,
            original_unlock_ts: unlock_ts,
            lock_start_ts,
            is_partial,
        };

        // --- persist history ----------------------------------------------
        let hist_key = EmergencyDataKey::History(staker.clone());
        let mut history: Vec<EmergencyUnstakeRecord> = env
            .storage()
            .persistent()
            .get(&hist_key)
            .unwrap_or_else(|| Vec::new(env));
        history.push_back(record.clone());
        env.storage().persistent().set(&hist_key, &history);

        // --- activate cooldown --------------------------------------------
        if config.cooldown_seconds > 0 {
            let new_cooldown_end = now + config.cooldown_seconds;
            env.storage().persistent().set(
                &EmergencyDataKey::CooldownEnd(staker.clone()),
                &new_cooldown_end,
            );
        }

        // --- emit unstake event ------------------------------------------
        env.events()
            .publish((symbol_short!("EMRGUNSTK"), staker.clone()), record.clone());

        Ok(record)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Emitted when a penalty is collected and earmarked for the treasury.
#[contracttype]
#[derive(Debug, Clone)]
pub struct PenaltyDistributionEvent {
    /// The staker from whom the penalty was deducted.
    pub staker: Address,
    /// The treasury address that receives the penalty.
    pub treasury: Address,
    /// Total penalty collected, in base units.
    pub penalty_amount: i128,
    /// Ledger timestamp of the event.
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Read-only helpers for querying emergency-unstake state.
pub struct EmergencyUnstakeQuery;

impl EmergencyUnstakeQuery {
    /// Load the contract's [`EmergencyUnstakeConfig`], if any.
    pub fn config(env: &Env) -> Option<EmergencyUnstakeConfig> {
        env.storage().persistent().get(&EmergencyDataKey::Config)
    }

    /// Return the ledger timestamp after which `staker` may emergency-unstake
    /// again. `0` means no active cooldown.
    pub fn cooldown_end(env: &Env, staker: &Address) -> u64 {
        env.storage()
            .persistent()
            .get(&EmergencyDataKey::CooldownEnd(staker.clone()))
            .unwrap_or(0)
    }

    /// Return `true` if `staker` is currently in a cooldown period.
    pub fn in_cooldown(env: &Env, staker: &Address) -> bool {
        let now = env.ledger().timestamp();
        Self::cooldown_end(env, staker) > now
    }

    /// The full, chronologically ordered emergency-unstake history for a staker.
    pub fn history(env: &Env, staker: &Address) -> Vec<EmergencyUnstakeRecord> {
        env.storage()
            .persistent()
            .get(&EmergencyDataKey::History(staker.clone()))
            .unwrap_or_else(|| Vec::new(env))
    }

    /// Preview the penalty basis points for a hypothetical emergency unstake
    /// without touching storage.
    pub fn preview_penalty_bps(env: &Env, lock_start_ts: u64, unlock_ts: u64) -> Option<i128> {
        let config: EmergencyUnstakeConfig =
            env.storage().persistent().get(&EmergencyDataKey::Config)?;
        let now = env.ledger().timestamp();
        let total_lock_seconds = unlock_ts.saturating_sub(lock_start_ts);
        let elapsed = now.saturating_sub(lock_start_ts);
        PenaltyCalculator::compute_penalty_bps(&config, elapsed, total_lock_seconds).ok()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    // Helper: create a minimal config for tests.
    fn test_config(
        start_bps: i128,
        end_bps: i128,
        decay: PenaltyDecayFunction,
    ) -> EmergencyUnstakeConfig {
        // Treasury address is irrelevant for pure math tests; a valid
        // placeholder is generated from a dummy Env. Note: `Address` cannot be
        // zero-initialized in this soroban-sdk version, so `core::mem::zeroed()`
        // (the previous hack) panics at first use.
        let env = Env::default();
        EmergencyUnstakeConfig {
            penalty_start_bps: start_bps,
            penalty_end_bps: end_bps,
            decay_function: decay,
            cooldown_seconds: 86_400,
            treasury: Address::generate(&env),
            enabled: true,
        }
    }

    // --- Linear decay tests -----------------------------------------------

    #[test]
    fn linear_start_penalty_at_elapsed_zero() {
        let cfg = test_config(3000, 500, PenaltyDecayFunction::Linear);
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, 0, 30 * 24 * 3600).unwrap();
        assert_eq!(bps, 3000, "at t=0, penalty must equal start");
    }

    #[test]
    fn linear_end_penalty_at_full_elapsed() {
        let cfg = test_config(3000, 500, PenaltyDecayFunction::Linear);
        let total = 30u64 * 24 * 3600;
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, total, total).unwrap();
        assert_eq!(bps, 500, "at t=total, penalty must equal end");
    }

    #[test]
    fn linear_midpoint_penalty() {
        // Midpoint of [3000, 500] should be (3000+500)/2 = 1750.
        let cfg = test_config(3000, 500, PenaltyDecayFunction::Linear);
        let total = 30u64 * 24 * 3600;
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, total / 2, total).unwrap();
        // tolerance of ±5 bps for fixed-point rounding
        let expected = 1750i128;
        let diff = (bps - expected).abs();
        assert!(
            diff <= 5,
            "mid-point linear penalty {} != {}",
            bps,
            expected
        );
    }

    #[test]
    fn linear_penalty_is_monotone_decreasing() {
        let cfg = test_config(5000, 0, PenaltyDecayFunction::Linear);
        let total = 90u64 * 24 * 3600;
        let mut prev = MAX_BPS;
        for pct in [0u64, 10, 25, 50, 75, 90, 100] {
            let elapsed = total * pct / 100;
            let bps = PenaltyCalculator::compute_penalty_bps(&cfg, elapsed, total).unwrap();
            assert!(
                bps <= prev,
                "penalty not decreasing at {}%: prev={}, now={}",
                pct,
                prev,
                bps
            );
            prev = bps;
        }
    }

    // --- Exponential decay tests ------------------------------------------

    #[test]
    fn exponential_start_penalty_at_elapsed_zero() {
        let cfg = test_config(3000, 500, PenaltyDecayFunction::Exponential);
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, 0, 30 * 24 * 3600).unwrap();
        assert_eq!(bps, 3000);
    }

    #[test]
    fn exponential_end_penalty_at_full_elapsed() {
        let cfg = test_config(3000, 500, PenaltyDecayFunction::Exponential);
        let total = 30u64 * 24 * 3600;
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, total, total).unwrap();
        assert_eq!(bps, 500);
    }

    #[test]
    fn exponential_midpoint_less_than_linear_midpoint() {
        // Exponential decay is convex (decays faster early), so the
        // mid-point penalty is lower than linear interpolation.
        let start = 4000i128;
        let end = 400i128;
        let total = 30u64 * 24 * 3600;
        let half = total / 2;

        let linear_mid = PenaltyCalculator::compute_penalty_bps(
            &test_config(start, end, PenaltyDecayFunction::Linear),
            half,
            total,
        )
        .unwrap();
        let exp_mid = PenaltyCalculator::compute_penalty_bps(
            &test_config(start, end, PenaltyDecayFunction::Exponential),
            half,
            total,
        )
        .unwrap();

        assert!(
            exp_mid < linear_mid,
            "exponential mid {} should be < linear mid {}",
            exp_mid,
            linear_mid
        );
    }

    #[test]
    fn exponential_penalty_is_monotone_decreasing() {
        let cfg = test_config(5000, 100, PenaltyDecayFunction::Exponential);
        let total = 90u64 * 24 * 3600;
        let mut prev = MAX_BPS;
        for pct in [0u64, 10, 25, 50, 75, 90, 100] {
            let elapsed = total * pct / 100;
            let bps = PenaltyCalculator::compute_penalty_bps(&cfg, elapsed, total).unwrap();
            assert!(
                bps <= prev,
                "exp penalty not decreasing at {}%: prev={}, now={}",
                pct,
                prev,
                bps
            );
            prev = bps;
        }
    }

    // --- Apply penalty tests ----------------------------------------------

    #[test]
    fn apply_zero_penalty_returns_full_amount() {
        let (penalty, net) = PenaltyCalculator::apply_penalty(1_000_000, 0).unwrap();
        assert_eq!(penalty, 0);
        assert_eq!(net, 1_000_000);
    }

    #[test]
    fn apply_full_penalty_returns_zero_net() {
        let (penalty, net) = PenaltyCalculator::apply_penalty(1_000_000, MAX_BPS).unwrap();
        assert_eq!(penalty, 1_000_000);
        assert_eq!(net, 0);
    }

    #[test]
    fn apply_ten_percent_penalty() {
        // 1000 bps = 10%. On 1_000_000 -> penalty=100_000, net=900_000.
        let (penalty, net) = PenaltyCalculator::apply_penalty(1_000_000, 1_000).unwrap();
        assert_eq!(penalty, 100_000);
        assert_eq!(net, 900_000);
    }

    #[test]
    fn apply_penalty_is_exact_on_round_number() {
        // 2500 bps = 25%. On 8_000 -> penalty=2_000, net=6_000.
        let (penalty, net) = PenaltyCalculator::apply_penalty(8_000, 2_500).unwrap();
        assert_eq!(penalty, 2_000);
        assert_eq!(net, 6_000);
    }

    // --- Custom decay test ------------------------------------------------

    #[test]
    fn custom_decay_always_returns_start() {
        let cfg = test_config(2500, 100, PenaltyDecayFunction::Custom);
        let total = 30u64 * 24 * 3600;
        for elapsed in [0u64, total / 4, total / 2, total * 3 / 4] {
            let bps = PenaltyCalculator::compute_penalty_bps(&cfg, elapsed, total).unwrap();
            assert_eq!(bps, 2500, "custom decay should always return start_bps");
        }
    }

    // --- Clamp tests ------------------------------------------------------

    #[test]
    fn penalty_clamped_to_max_bps() {
        let cfg = test_config(20_000, 0, PenaltyDecayFunction::Linear); // out-of-range start
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, 0, 86_400).unwrap();
        assert_eq!(bps, MAX_BPS, "penalty must be clamped at MAX_BPS");
    }

    #[test]
    fn penalty_clamped_to_zero() {
        let cfg = test_config(1000, -500, PenaltyDecayFunction::Linear); // negative end
        let total = 86_400u64;
        let bps = PenaltyCalculator::compute_penalty_bps(&cfg, total, total).unwrap();
        assert_eq!(bps, 0, "penalty must be clamped at 0");
    }
}
