#![no_std]
//! # AstraPort Portfolio Valuation & Performance Metrics Contract
//!
//! Provides comprehensive portfolio valuation and performance tracking:
//!
//! - Total portfolio value calculation (sum of all asset values at current prices)
//! - Asset allocation percentages (must sum to 100% ± 0.1%)
//! - Absolute and percentage returns vs. initial investment
//! - Performance metrics: Sharpe ratio, Sortino ratio, maximum drawdown
//! - Time-weighted return (TWR) for accurate performance measurement
//! - Portfolio snapshots for historical comparison
//! - Valuation history with timestamps for trend analysis
//!
//! ## Modules
//!
//! - [`records`] — Soroban-typed data structures for assets, snapshots,
//!   history entries, and performance metrics.
//! - [`performance`] — Pure-function fixed-point implementations of Sharpe
//!   ratio, Sortino ratio, maximum drawdown, and time-weighted return.

use soroban_sdk::{contract, contracterror, contractimpl, symbol_short, Env, Map, Symbol, Vec};

pub mod performance;
pub mod records;

use crate::performance::{fp_div, PerfError};
use crate::records::{
    AssetAllocation, PerformanceMetrics, PortfolioAsset, PortfolioReturns, PortfolioSnapshot,
    ValuationDataKey, ValuationHistoryEntry, SCALE,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the valuation contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ValuationError {
    /// The portfolio has no assets.
    EmptyPortfolio = 1,
    /// Initial investment is zero (cannot compute percentage returns).
    ZeroInitialInvestment = 2,
    /// Insufficient data for the requested metric.
    InsufficientData = 3,
    /// Internal math error (overflow, division by zero).
    MathError = 4,
    /// The portfolio does not exist (no assets registered).
    PortfolioNotFound = 5,
}

impl From<PerfError> for ValuationError {
    fn from(e: PerfError) -> Self {
        match e {
            PerfError::DivideByZero => ValuationError::MathError,
            PerfError::InsufficientData => ValuationError::InsufficientData,
            PerfError::Overflow => ValuationError::MathError,
        }
    }
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Portfolio valuation and performance metrics contract for AstraPort.
#[contract]
pub struct ValuationContract;

#[contractimpl]
impl ValuationContract {
    // -------------------------------------------------------------------
    // Initialization
    // -------------------------------------------------------------------

    /// Initialize the valuation contract.
    ///
    /// Sets the default risk-free rate and annualization period.
    pub fn initialize(env: Env) -> Symbol {
        let storage = env.storage().persistent();
        if !storage.has(&ValuationDataKey::RiskFreeRate) {
            // Default risk-free rate: 4% annualized (0.04 in fixed-point)
            storage.set(&ValuationDataKey::RiskFreeRate, &(4 * SCALE / 100));
        }
        if !storage.has(&ValuationDataKey::AnnualizationPeriod) {
            // Default: 365 days (daily returns)
            storage.set(&ValuationDataKey::AnnualizationPeriod, &(365u64));
        }
        symbol_short!("ok")
    }

    // -------------------------------------------------------------------
    // Configuration
    // -------------------------------------------------------------------

    /// Set the annualized risk-free rate used in Sharpe/Sortino calculations.
    ///
    /// `rate` is a decimal fraction in fixed-point (e.g. 0.04 for 4%).
    pub fn set_risk_free_rate(env: Env, rate: i128) {
        env.storage()
            .persistent()
            .set(&ValuationDataKey::RiskFreeRate, &rate);
    }

    /// Get the current annualized risk-free rate.
    pub fn get_risk_free_rate(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&ValuationDataKey::RiskFreeRate)
            .unwrap_or(4 * SCALE / 100)
    }

    /// Set the number of periods per year for annualization (e.g. 365 for daily).
    pub fn set_annualization_period(env: Env, periods: u64) {
        env.storage()
            .persistent()
            .set(&ValuationDataKey::AnnualizationPeriod, &periods);
    }

    /// Get the current annualization period.
    pub fn get_annualization_period(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&ValuationDataKey::AnnualizationPeriod)
            .unwrap_or(365)
    }

    // -------------------------------------------------------------------
    // Asset management
    // -------------------------------------------------------------------

    /// Add or update an asset in a portfolio.
    ///
    /// If the asset already exists its quantity, price, and cost basis are
    /// updated. Otherwise a new entry is appended.
    pub fn set_asset(
        env: Env,
        portfolio_id: Symbol,
        asset: Symbol,
        quantity: i128,
        current_price: i128,
        cost_basis: i128,
    ) {
        let key = ValuationDataKey::Assets(portfolio_id.clone());
        let mut assets: Vec<PortfolioAsset> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        // Check if asset already exists and update it
        let mut found = false;
        for i in 0..assets.len() {
            let mut a = assets.get(i).unwrap();
            if a.asset == asset {
                a.quantity = quantity;
                a.current_price = current_price;
                a.cost_basis = cost_basis;
                // Vec doesn't support in-place update easily, so we
                // remove and re-insert at the same position.
                assets.remove(i);
                assets.insert(i, a);
                found = true;
                break;
            }
        }

        if !found {
            assets.push_back(PortfolioAsset {
                asset,
                quantity,
                current_price,
                cost_basis,
            });
        }

        env.storage().persistent().set(&key, &assets);
    }

    /// Remove an asset from a portfolio.
    ///
    /// Returns `true` if the asset was found and removed.
    pub fn remove_asset(env: Env, portfolio_id: Symbol, asset: Symbol) -> bool {
        let key = ValuationDataKey::Assets(portfolio_id);
        let mut assets: Vec<PortfolioAsset> = match env.storage().persistent().get(&key) {
            Some(a) => a,
            None => return false,
        };

        for i in 0..assets.len() {
            if assets.get(i).unwrap().asset == asset {
                assets.remove(i);
                env.storage().persistent().set(&key, &assets);
                return true;
            }
        }
        false
    }

    /// Set the initial (total cost basis) investment for a portfolio.
    pub fn set_initial_investment(env: Env, portfolio_id: Symbol, amount: i128) {
        let key = ValuationDataKey::InitialInvestment(portfolio_id);
        env.storage().persistent().set(&key, &amount);
    }

    /// Get the initial investment for a portfolio.
    pub fn get_initial_investment(env: Env, portfolio_id: Symbol) -> i128 {
        let key = ValuationDataKey::InitialInvestment(portfolio_id);
        env.storage().persistent().get(&key).unwrap_or(0)
    }

    // -------------------------------------------------------------------
    // Portfolio valuation
    // -------------------------------------------------------------------

    /// Calculate the total value of a portfolio by summing all asset values
    /// at current prices.
    ///
    /// Total value = Σ(quantity × current_price) for each asset.
    /// Returns 0 for an empty or non-existent portfolio.
    pub fn calculate_portfolio_value(env: Env, portfolio_id: Symbol) -> i128 {
        let assets = Self::get_assets(env, portfolio_id);
        let mut total = 0i128;

        for i in 0..assets.len() {
            let a = assets.get(i).unwrap();
            // value = quantity * current_price
            // quantity is integer, current_price is fixed-point
            let value = a.quantity * a.current_price;
            total = total.checked_add(value).unwrap_or(i128::MAX);
        }

        total
    }

    /// Get the list of assets in a portfolio.
    pub fn get_assets(env: Env, portfolio_id: Symbol) -> Vec<PortfolioAsset> {
        let key = ValuationDataKey::Assets(portfolio_id);
        env.storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // -------------------------------------------------------------------
    // Asset allocation
    // -------------------------------------------------------------------

    /// Get the current allocation of each asset as a percentage of total
    /// portfolio value.
    ///
    /// Percentages are returned in basis points (1 = 0.01%, 10000 = 100%).
    /// The sum of all allocations is guaranteed to be 10000 ± 10 basis points.
    ///
    /// Returns an error for empty portfolios.
    pub fn get_asset_allocation(
        env: Env,
        portfolio_id: Symbol,
    ) -> Result<Vec<AssetAllocation>, ValuationError> {
        let assets = Self::get_assets(env.clone(), portfolio_id.clone());
        if assets.is_empty() {
            return Err(ValuationError::EmptyPortfolio);
        }

        let total_value = Self::calculate_portfolio_value(env.clone(), portfolio_id);
        if total_value <= 0 {
            return Err(ValuationError::EmptyPortfolio);
        }

        let mut result = Vec::new(&env);
        let mut allocated_bps = 0u32;

        for i in 0..assets.len() {
            let a = assets.get(i).unwrap();
            let market_value = a.quantity * a.current_price;

            // allocation_bps = (market_value / total_value) * 10000
            // Using fixed-point: (market_value * 10000 * SCALE) / (total_value * SCALE)
            // Simplifies to: market_value * 10000 / total_value
            let bps = if total_value > 0 {
                let raw = market_value.checked_mul(10_000).unwrap_or(0) / total_value;
                raw.max(0) as u32
            } else {
                0
            };

            allocated_bps = allocated_bps.saturating_add(bps);

            result.push_back(AssetAllocation {
                asset: a.asset.clone(),
                market_value,
                allocation_bps: bps,
            });
        }

        // Adjust rounding error on the last asset to hit exactly 10000
        if !result.is_empty() && allocated_bps != 10_000 {
            let last_idx = result.len() - 1;
            let mut last = result.get(last_idx).unwrap();
            let diff = 10_000_i64 - allocated_bps as i64;
            last.allocation_bps = (last.allocation_bps as i64 + diff).max(0) as u32;
            result.remove(last_idx);
            result.push_back(last);
        }

        Ok(result)
    }

    // -------------------------------------------------------------------
    // Returns calculation
    // -------------------------------------------------------------------

    /// Calculate absolute and percentage returns for a portfolio.
    ///
    /// Absolute return = current_value − initial_investment
    /// Percentage return = absolute_return / initial_investment
    ///
    /// Returns an error if the initial investment is zero or the portfolio
    /// is empty.
    pub fn calculate_returns(
        env: Env,
        portfolio_id: Symbol,
    ) -> Result<PortfolioReturns, ValuationError> {
        let current_value = Self::calculate_portfolio_value(env.clone(), portfolio_id.clone());
        let initial = Self::get_initial_investment(env, portfolio_id);

        if initial == 0 {
            return Err(ValuationError::ZeroInitialInvestment);
        }
        if current_value == 0 {
            return Err(ValuationError::EmptyPortfolio);
        }

        let absolute = current_value - initial;
        let percentage = fp_div(absolute, initial).map_err(ValuationError::from)?;

        Ok(PortfolioReturns {
            absolute_return: absolute,
            percentage_return: percentage,
        })
    }

    // -------------------------------------------------------------------
    // Performance metrics
    // -------------------------------------------------------------------

    /// Calculate comprehensive performance metrics for a portfolio.
    ///
    /// Requires at least 2 valuation history entries (daily snapshots).
    /// Uses the configured risk-free rate and annualization period.
    pub fn calculate_performance_metrics(
        env: Env,
        portfolio_id: Symbol,
    ) -> Result<PerformanceMetrics, ValuationError> {
        let history_key = ValuationDataKey::ValuationHistory(portfolio_id);
        let history: Vec<ValuationHistoryEntry> = env
            .storage()
            .persistent()
            .get(&history_key)
            .unwrap_or_else(|| Vec::new(&env));

        if history.len() < 2 {
            return Err(ValuationError::InsufficientData);
        }

        // Build daily returns series from valuation history
        let mut periodic_returns: Vec<i128> = Vec::new(&env);
        for i in 1..history.len() {
            let prev_val = history.get(i - 1).unwrap().total_value;
            let curr_val = history.get(i).unwrap().total_value;
            if prev_val > 0 {
                let ret = fp_div(curr_val - prev_val, prev_val).map_err(ValuationError::from)?;
                periodic_returns.push_back(ret);
            }
        }

        if periodic_returns.len() < 2 {
            return Err(ValuationError::InsufficientData);
        }

        let rf = Self::get_risk_free_rate(env.clone());
        let periods = Self::get_annualization_period(env.clone());

        // Sharpe ratio
        let sharpe = performance::sharpe_ratio(&periodic_returns, rf, periods)
            .map_err(ValuationError::from)?;

        // Sortino ratio
        let sortino = performance::sortino_ratio(&periodic_returns, rf, periods)
            .map_err(ValuationError::from)?;

        // Maximum drawdown from valuation history
        let mut valuations: Vec<i128> = Vec::new(&env);
        for i in 0..history.len() {
            valuations.push_back(history.get(i).unwrap().total_value);
        }
        let dd = performance::max_drawdown(&valuations).map_err(ValuationError::from)?;

        // Time-weighted return
        let twr = performance::time_weighted_return(&valuations).map_err(ValuationError::from)?;

        Ok(PerformanceMetrics {
            sharpe_ratio: sharpe,
            sortino_ratio: sortino,
            max_drawdown: dd,
            time_weighted_return: twr,
        })
    }

    // -------------------------------------------------------------------
    // Snapshots
    // -------------------------------------------------------------------

    /// Take a snapshot of the current portfolio state.
    ///
    /// Records the current total value, cost basis, and asset count with
    /// the current ledger timestamp. Also appends a valuation history entry.
    pub fn take_snapshot(env: Env, portfolio_id: Symbol) -> PortfolioSnapshot {
        let total_value = Self::calculate_portfolio_value(env.clone(), portfolio_id.clone());
        let total_cost_basis = Self::calculate_total_cost_basis(env.clone(), portfolio_id.clone());
        let assets = Self::get_assets(env.clone(), portfolio_id.clone());
        let timestamp = env.ledger().timestamp();

        let snapshot = PortfolioSnapshot {
            timestamp,
            total_value,
            total_cost_basis,
            asset_count: assets.len(),
        };

        // Store latest snapshot
        let latest_key = ValuationDataKey::LatestSnapshot(portfolio_id.clone());
        env.storage().persistent().set(&latest_key, &snapshot);

        // Append to snapshot history
        let history_key = ValuationDataKey::Snapshots(portfolio_id.clone());
        let mut snapshots: Vec<PortfolioSnapshot> = env
            .storage()
            .persistent()
            .get(&history_key)
            .unwrap_or_else(|| Vec::new(&env));
        snapshots.push_back(snapshot.clone());
        env.storage().persistent().set(&history_key, &snapshots);

        // Build per-asset values for valuation history
        let mut asset_values: Map<Symbol, i128> = Map::new(&env);
        let mut abs_return = 0i128;
        let mut pct_return = 0i128;

        for i in 0..assets.len() {
            let a = assets.get(i).unwrap();
            let value = a.quantity * a.current_price;
            asset_values.set(a.asset.clone(), value);
        }

        if total_cost_basis > 0 {
            abs_return = total_value - total_cost_basis;
            if let Ok(p) = fp_div(abs_return, total_cost_basis) {
                pct_return = p;
            }
        }

        // Append valuation history entry
        let vh_key = ValuationDataKey::ValuationHistory(portfolio_id.clone());
        let mut vh: Vec<ValuationHistoryEntry> = env
            .storage()
            .persistent()
            .get(&vh_key)
            .unwrap_or_else(|| Vec::new(&env));
        vh.push_back(ValuationHistoryEntry {
            timestamp,
            total_value,
            absolute_return: abs_return,
            percentage_return: pct_return,
            asset_values,
        });
        env.storage().persistent().set(&vh_key, &vh);

        snapshot
    }

    /// Get the most recent snapshot for a portfolio.
    pub fn get_latest_snapshot(env: Env, portfolio_id: Symbol) -> Option<PortfolioSnapshot> {
        let key = ValuationDataKey::LatestSnapshot(portfolio_id);
        env.storage().persistent().get(&key)
    }

    /// Get the complete snapshot history for a portfolio.
    pub fn get_snapshot_history(env: Env, portfolio_id: Symbol) -> Vec<PortfolioSnapshot> {
        let key = ValuationDataKey::Snapshots(portfolio_id);
        env.storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Compare two snapshots to show portfolio evolution.
    ///
    /// Returns `(value_change, percentage_change, time_delta_seconds)`.
    pub fn compare_snapshots(
        env: Env,
        portfolio_id: Symbol,
        older_index: u32,
        newer_index: u32,
    ) -> Result<(i128, i128, u64), ValuationError> {
        let snapshots = Self::get_snapshot_history(env, portfolio_id);
        if older_index >= snapshots.len() || newer_index >= snapshots.len() {
            return Err(ValuationError::InsufficientData);
        }
        if older_index >= newer_index {
            return Err(ValuationError::InsufficientData);
        }

        let older = snapshots.get(older_index).unwrap();
        let newer = snapshots.get(newer_index).unwrap();

        let value_change = newer.total_value - older.total_value;
        let pct_change = if older.total_value > 0 {
            fp_div(value_change, older.total_value).map_err(ValuationError::from)?
        } else {
            0
        };
        let time_delta = newer.timestamp.saturating_sub(older.timestamp);

        Ok((value_change, pct_change, time_delta))
    }

    // -------------------------------------------------------------------
    // Valuation history
    // -------------------------------------------------------------------

    /// Get the complete valuation history for a portfolio.
    pub fn get_valuation_history(env: Env, portfolio_id: Symbol) -> Vec<ValuationHistoryEntry> {
        let key = ValuationDataKey::ValuationHistory(portfolio_id);
        env.storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Calculate total cost basis (sum of quantity × cost_basis for each asset).
    fn calculate_total_cost_basis(env: Env, portfolio_id: Symbol) -> i128 {
        let assets = Self::get_assets(env, portfolio_id);
        let mut total = 0i128;
        for i in 0..assets.len() {
            let a = assets.get(i).unwrap();
            total = total
                .checked_add(a.quantity * a.cost_basis)
                .unwrap_or(i128::MAX);
        }
        total
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _, Env};

    fn approx(a: i128, b: i128, tol: i128) {
        let diff = (a - b).abs();
        assert!(
            diff <= tol,
            "expected {} ~= {} within {}, diff was {}",
            a,
            b,
            tol,
            diff
        );
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        let result = ValuationContract::initialize(env);
        assert_eq!(result, symbol_short!("ok"));
    }

    #[test]
    fn test_set_and_get_asset() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        ValuationContract::set_asset(
            env.clone(),
            pid.clone(),
            xlm.clone(),
            1000,      // quantity
            SCALE,     // price = 1.0
            SCALE / 2, // cost basis = 0.5
        );

        let assets = ValuationContract::get_assets(env, pid);
        assert_eq!(assets.len(), 1);
        let a = assets.get(0).unwrap();
        assert_eq!(a.asset, xlm);
        assert_eq!(a.quantity, 1000);
        assert_eq!(a.current_price, SCALE);
    }

    #[test]
    fn test_update_asset() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        ValuationContract::set_asset(env.clone(), pid.clone(), xlm.clone(), 1000, SCALE, SCALE);
        ValuationContract::set_asset(
            env.clone(),
            pid.clone(),
            xlm.clone(),
            2000,
            SCALE * 2,
            SCALE,
        );

        let assets = ValuationContract::get_assets(env, pid);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets.get(0).unwrap().quantity, 2000);
        assert_eq!(assets.get(0).unwrap().current_price, SCALE * 2);
    }

    #[test]
    fn test_remove_asset() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        ValuationContract::set_asset(env.clone(), pid.clone(), xlm.clone(), 1000, SCALE, SCALE);
        assert!(ValuationContract::remove_asset(
            env.clone(),
            pid.clone(),
            xlm.clone()
        ));
        assert!(ValuationContract::get_assets(env, pid).is_empty());
    }

    #[test]
    fn test_calculate_portfolio_value_single_asset() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        // 1000 units at $2.00 each = $2000
        ValuationContract::set_asset(env.clone(), pid.clone(), xlm, 1000, 2 * SCALE, SCALE);

        let value = ValuationContract::calculate_portfolio_value(env, pid);
        assert_eq!(value, 2000 * SCALE);
    }

    #[test]
    fn test_calculate_portfolio_value_multiple_assets() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");
        let usdc = symbol_short!("USDC");

        // 1000 XLM at $1.00 = $1000
        ValuationContract::set_asset(env.clone(), pid.clone(), xlm, 1000, SCALE, SCALE);
        // 500 USDC at $2.00 = $1000
        ValuationContract::set_asset(env.clone(), pid.clone(), usdc, 500, 2 * SCALE, SCALE);

        let value = ValuationContract::calculate_portfolio_value(env, pid);
        assert_eq!(value, 2000 * SCALE);
    }

    #[test]
    fn test_calculate_portfolio_value_empty() {
        let env = Env::default();
        let pid = symbol_short!("port1");
        let value = ValuationContract::calculate_portfolio_value(env, pid);
        assert_eq!(value, 0);
    }

    #[test]
    fn test_asset_allocation() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");
        let usdc = symbol_short!("USDC");

        // XLM: 1000 * $1.00 = $1000, USDC: 500 * $2.00 = $1000
        // Each should be 50%
        ValuationContract::set_asset(env.clone(), pid.clone(), xlm, 1000, SCALE, SCALE);
        ValuationContract::set_asset(env.clone(), pid.clone(), usdc, 500, 2 * SCALE, SCALE);

        let alloc = ValuationContract::get_asset_allocation(env, pid).unwrap();
        assert_eq!(alloc.len(), 2);

        let total_bps: u32 = alloc.iter().map(|a| a.allocation_bps).sum();
        assert!(
            (total_bps as i64 - 10_000).unsigned_abs() <= 10,
            "Allocation should sum to ~10000, got {}",
            total_bps
        );

        // Both should be approximately 5000 bps
        for a in alloc.iter() {
            assert!(
                (a.allocation_bps as i64 - 5_000).unsigned_abs() <= 10,
                "Each allocation should be ~5000 bps, got {} for {}",
                a.allocation_bps,
                a.asset
            );
        }
    }

    #[test]
    fn test_asset_allocation_empty() {
        let env = Env::default();
        let pid = symbol_short!("port1");
        assert_eq!(
            ValuationContract::get_asset_allocation(env, pid),
            Err(ValuationError::EmptyPortfolio)
        );
    }

    #[test]
    fn test_calculate_returns() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        // Initial investment: $1000
        ValuationContract::set_initial_investment(env.clone(), pid.clone(), 1000 * SCALE);
        // Current value: 1000 units at $1.50 = $1500
        ValuationContract::set_asset(
            env.clone(),
            pid.clone(),
            xlm,
            1000,
            SCALE + SCALE / 2,
            SCALE,
        );

        let returns = ValuationContract::calculate_returns(env, pid).unwrap();
        assert_eq!(returns.absolute_return, 500 * SCALE);
        approx(returns.percentage_return, SCALE / 2, SCALE / 1000); // 50%
    }

    #[test]
    fn test_calculate_returns_zero_investment() {
        let env = Env::default();
        let pid = symbol_short!("port1");
        assert_eq!(
            ValuationContract::calculate_returns(env, pid),
            Err(ValuationError::ZeroInitialInvestment)
        );
    }

    #[test]
    fn test_take_and_compare_snapshots() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        ValuationContract::set_initial_investment(env.clone(), pid.clone(), 1000 * SCALE);

        // Snapshot 1: price = $1.00
        ValuationContract::set_asset(env.clone(), pid.clone(), xlm.clone(), 1000, SCALE, SCALE);
        let s1 = ValuationContract::take_snapshot(env.clone(), pid.clone());
        assert_eq!(s1.total_value, 1000 * SCALE);

        // Snapshot 2: price = $1.50
        ValuationContract::set_asset(
            env.clone(),
            pid.clone(),
            xlm.clone(),
            1000,
            SCALE + SCALE / 2,
            SCALE,
        );
        let s2 = ValuationContract::take_snapshot(env.clone(), pid.clone());
        assert_eq!(s2.total_value, 1500 * SCALE);

        // Compare
        let (change, pct, _) =
            ValuationContract::compare_snapshots(env.clone(), pid, 0, 1).unwrap();
        assert_eq!(change, 500 * SCALE);
        approx(pct, SCALE / 2, SCALE / 1000); // 50%
    }

    #[test]
    fn test_valuation_history() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");

        ValuationContract::set_initial_investment(env.clone(), pid.clone(), 1000 * SCALE);
        ValuationContract::set_asset(env.clone(), pid.clone(), xlm, 1000, SCALE, SCALE);

        ValuationContract::take_snapshot(env.clone(), pid.clone());

        // Advance time
        let mut ledger = env.ledger().get();
        ledger.timestamp += 86400;
        env.ledger().set(ledger);

        ValuationContract::take_snapshot(env.clone(), pid.clone());

        let history = ValuationContract::get_valuation_history(env, pid);
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0).unwrap().total_value, 1000 * SCALE);
        assert_eq!(history.get(1).unwrap().total_value, 1000 * SCALE);
    }

    #[test]
    fn test_risk_free_rate_config() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());

        // Default should be 4%
        let rf = ValuationContract::get_risk_free_rate(env.clone());
        assert_eq!(rf, 4 * SCALE / 100);

        // Set to 3%
        ValuationContract::set_risk_free_rate(env.clone(), 3 * SCALE / 100);
        assert_eq!(ValuationContract::get_risk_free_rate(env), 3 * SCALE / 100);
    }

    #[test]
    fn test_performance_metrics_insufficient_data() {
        let env = Env::default();
        ValuationContract::initialize(env.clone());
        let pid = symbol_short!("port1");

        assert_eq!(
            ValuationContract::calculate_performance_metrics(env, pid),
            Err(ValuationError::InsufficientData)
        );
    }

    #[test]
    fn test_remove_nonexistent_asset() {
        let env = Env::default();
        let pid = symbol_short!("port1");
        let xlm = symbol_short!("XLM");
        assert!(!ValuationContract::remove_asset(env, pid, xlm));
    }
}
