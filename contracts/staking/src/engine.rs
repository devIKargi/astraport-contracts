//! The storage-backed yield engine.
//!
//! [`YieldEngine`] is the stateful layer that sits on top of the pure math in
//! [`crate::compounding`] / [`crate::apy`] and the projection helper in
//! [`crate::projection`]. It owns the persistence of [`YieldRecord`]s,
//! append-only [`YieldHistoryEntry`] logs, and [`DistributionSchedule`]s, and it
//! implements **real-time accrual**: yield is checkpointed against the ledger
//! clock so a position's earnings are always current when queried.
//!
//! Rate changes are handled by checkpointing accrued yield at the old rate
//! *before* applying the new rate, which makes the history time-weighted and
//! exact across rate boundaries.

use soroban_sdk::{Address, Env, Symbol, Vec};

use crate::compounding::YieldCalculator;
use crate::fixed_point::MathError;
use crate::records::{
    CompoundingMode, DistributionSchedule, DistributionType, YieldDataKey, YieldDistributionRecord,
    YieldHistoryEntry, YieldRecord,
};

/// Stateful engine coordinating yield accrual, history, and distributions.
///
/// The engine is a thin, `env`-scoped facade; it holds no state itself beyond
/// the borrowed [`Env`] and reads/writes everything through persistent storage.
pub struct YieldEngine<'a> {
    env: &'a Env,
}

impl<'a> YieldEngine<'a> {
    /// Create an engine bound to the given environment.
    pub fn new(env: &'a Env) -> Self {
        Self { env }
    }

    /// Open or create a position for `(staker, asset)` with the given principal,
    /// rate, and compounding mode.
    ///
    /// If a record already exists it is first brought current (accrued to now)
    /// and then overwritten with the new principal/rate, preserving accrued
    /// yield. The starting timestamp is the current ledger time.
    pub fn open_position(
        &self,
        staker: &Address,
        asset: &Symbol,
        principal: i128,
        apr: i128,
        mode: CompoundingMode,
    ) -> Result<YieldRecord, MathError> {
        let now = self.env.ledger().timestamp();

        // If a position exists, checkpoint it first so nothing is lost.
        let accrued = match self.load_record(staker, asset) {
            Some(existing) => {
                let updated = self.accrue_to(&existing, now)?;
                updated.accrued_yield
            }
            None => 0,
        };

        let record = YieldRecord {
            staker: staker.clone(),
            asset: asset.clone(),
            principal,
            apr,
            mode,
            last_accrual_ts: now,
            accrued_yield: accrued,
        };
        self.store_record(&record);
        Ok(record)
    }

    /// Bring a position's accrued yield current as of `now`, appending a history
    /// entry for the elapsed period and persisting the updated record.
    ///
    /// Returns the updated record. Accruing to a timestamp at or before the last
    /// checkpoint is a no-op (returns the record unchanged) so repeated calls in
    /// the same ledger are safe.
    pub fn accrue(&self, staker: &Address, asset: &Symbol) -> Result<YieldRecord, MathError> {
        let now = self.env.ledger().timestamp();
        let record = self
            .load_record(staker, asset)
            .ok_or(MathError::NegativeInput)?; // treat "missing" as invalid input
        let updated = self.accrue_to(&record, now)?;
        self.store_record(&updated);
        Ok(updated)
    }

    /// Adjust the principal of an existing position to `new_principal`,
    /// preserving its APR, compounding mode, and realized `accrued_yield`.
    ///
    /// The position is checkpointed (accrued to now) *before* the principal
    /// changes, so all yield earned on the old principal is realized and no yield
    /// is lost across the boundary. This is what stake/unstake use to keep a
    /// position's principal equal to the staked balance without resetting its
    /// rate.
    pub fn set_principal(
        &self,
        staker: &Address,
        asset: &Symbol,
        new_principal: i128,
    ) -> Result<YieldRecord, MathError> {
        let now = self.env.ledger().timestamp();
        let record = self
            .load_record(staker, asset)
            .ok_or(MathError::NegativeInput)?;
        // Realize everything earned on the old principal first.
        let mut updated = self.accrue_to(&record, now)?;
        // Then move principal going forward.
        updated.principal = new_principal;
        self.store_record(&updated);
        Ok(updated)
    }

    /// Change the APR for a position, checkpointing accrued yield at the old rate
    /// first so the transition is exact and time-weighted.
    pub fn set_rate(
        &self,
        staker: &Address,
        asset: &Symbol,
        new_apr: i128,
    ) -> Result<YieldRecord, MathError> {
        let now = self.env.ledger().timestamp();
        let record = self
            .load_record(staker, asset)
            .ok_or(MathError::NegativeInput)?;
        // Accrue everything earned at the OLD rate up to now.
        let mut updated = self.accrue_to(&record, now)?;
        // Then swap in the new rate going forward.
        updated.apr = new_apr;
        self.store_record(&updated);
        Ok(updated)
    }

    /// The total yield a position has earned *right now* — checkpointed accrued
    /// yield plus the yet-uncheckpointed amount since the last accrual.
    ///
    /// This is a read-only view; it does not mutate storage.
    pub fn current_yield(&self, staker: &Address, asset: &Symbol) -> Result<i128, MathError> {
        let now = self.env.ledger().timestamp();
        let record = self
            .load_record(staker, asset)
            .ok_or(MathError::NegativeInput)?;
        let pending = self.pending_yield(&record, now)?;
        record
            .accrued_yield
            .checked_add(pending)
            .ok_or(MathError::Overflow)
    }

    /// Compute yield earned since `record.last_accrual_ts` up to `now`, without
    /// mutating anything. Yield compounds on principal only (realized yield is
    /// tracked separately, matching a claim-based staking model).
    fn pending_yield(&self, record: &YieldRecord, now: u64) -> Result<i128, MathError> {
        if now <= record.last_accrual_ts {
            return Ok(0);
        }
        let elapsed = now - record.last_accrual_ts;
        let calc = YieldCalculator::new(record.mode.to_strategy());
        calc.compute_yield(record.principal, record.apr, elapsed)
    }

    /// Internal: produce a record advanced to `now`, appending a history entry
    /// for the elapsed period. Does not persist — callers store the result.
    fn accrue_to(&self, record: &YieldRecord, now: u64) -> Result<YieldRecord, MathError> {
        if now <= record.last_accrual_ts {
            return Ok(record.clone());
        }
        let elapsed = now - record.last_accrual_ts;
        let earned = self.pending_yield(record, now)?;
        let cumulative = record
            .accrued_yield
            .checked_add(earned)
            .ok_or(MathError::Overflow)?;

        // Append to the history log.
        self.append_history(
            &record.staker,
            &record.asset,
            YieldHistoryEntry {
                timestamp: now,
                period_seconds: elapsed,
                apr: record.apr,
                yield_earned: earned,
                cumulative_yield: cumulative,
                is_claim: false,
            },
        );

        let mut updated = record.clone();
        updated.accrued_yield = cumulative;
        updated.last_accrual_ts = now;
        Ok(updated)
    }

    /// Finalize a claim after [`Self::accrue`] has checkpointed the position.
    ///
    /// The marker is deliberately zero-period: the accrual entry (if any)
    /// remains an immutable account of what was earned, while this entry records
    /// that the accumulated amount was paid out and resets the unclaimed total.
    pub(crate) fn finalize_claim(&self, mut record: YieldRecord) -> i128 {
        let claimed = record.accrued_yield;
        record.accrued_yield = 0;

        self.append_history(
            &record.staker,
            &record.asset,
            YieldHistoryEntry {
                timestamp: self.env.ledger().timestamp(),
                period_seconds: 0,
                apr: record.apr,
                yield_earned: 0,
                cumulative_yield: 0,
                is_claim: true,
            },
        );
        self.store_record(&record);
        claimed
    }

    // --- history ---------------------------------------------------------

    /// The full yield history for a `(staker, asset)` pair, oldest first.
    pub fn history(&self, staker: &Address, asset: &Symbol) -> Vec<YieldHistoryEntry> {
        let key = YieldDataKey::History(staker.clone(), asset.clone());
        self.env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env))
    }

    /// Append an entry to a pair's history log.
    fn append_history(&self, staker: &Address, asset: &Symbol, entry: YieldHistoryEntry) {
        let key = YieldDataKey::History(staker.clone(), asset.clone());
        let mut log: Vec<YieldHistoryEntry> = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env));
        log.push_back(entry);
        self.env.storage().persistent().set(&key, &log);
    }

    // --- distribution scheduling ----------------------------------------

    /// Schedule a yield distribution for `(staker, asset)`.
    ///
    /// `interval_seconds` of 0 makes it a one-off; otherwise the schedule is
    /// recurring and [`Self::process_distribution`] rolls `due_ts` forward by the
    /// interval each time it fires.
    pub fn schedule_distribution(
        &self,
        staker: &Address,
        asset: &Symbol,
        amount: i128,
        due_ts: u64,
        interval_seconds: u64,
    ) -> DistributionSchedule {
        let schedule = DistributionSchedule {
            staker: staker.clone(),
            asset: asset.clone(),
            due_ts,
            interval_seconds,
            amount,
            executed: false,
        };
        let key = YieldDataKey::Schedule(staker.clone(), asset.clone());
        let mut list: Vec<DistributionSchedule> = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env));
        list.push_back(schedule.clone());
        self.env.storage().persistent().set(&key, &list);
        schedule
    }

    /// All distribution schedules for a `(staker, asset)` pair.
    pub fn schedules(&self, staker: &Address, asset: &Symbol) -> Vec<DistributionSchedule> {
        let key = YieldDataKey::Schedule(staker.clone(), asset.clone());
        self.env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env))
    }

    // --- record persistence ---------------------------------------------

    /// Load the active record for a pair, if any.
    pub fn load_record(&self, staker: &Address, asset: &Symbol) -> Option<YieldRecord> {
        let key = YieldDataKey::Record(staker.clone(), asset.clone());
        self.env.storage().persistent().get(&key)
    }

    /// Persist a record.
    fn store_record(&self, record: &YieldRecord) {
        let key = YieldDataKey::Record(record.staker.clone(), record.asset.clone());
        self.env.storage().persistent().set(&key, record);
    }

    // --- yield escrow / reserve ---------------------------------------

    /// Fund the yield reserve for an asset.
    ///
    /// Increases [`YieldDataKey::ReserveBalance`] by `amount`. The caller
    /// is responsible for the actual token transfer; this only updates the
    /// bookkeeping.
    pub fn fund_reserve(&self, asset: &Symbol, amount: i128) -> i128 {
        assert!(amount > 0, "ReserveFundAmountMustBePositive");
        let key = YieldDataKey::ReserveBalance(asset.clone());
        let current: i128 = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_default();
        let new_balance = current
            .checked_add(amount)
            .expect("ReserveBalance overflow");
        self.env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Return the current yield reserve balance for an asset.
    pub fn reserve_balance(&self, asset: &Symbol) -> i128 {
        let key = YieldDataKey::ReserveBalance(asset.clone());
        self.env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_default()
    }

    /// Withdraw from the yield reserve (admin-only caller verified in lib.rs).
    ///
    /// Decreases the reserve by `amount` and returns the new balance.
    /// Panics if the reserve is insufficient.
    pub fn withdraw_reserve(&self, asset: &Symbol, amount: i128) -> i128 {
        assert!(amount > 0, "ReserveWithdrawAmountMustBePositive");
        let key = YieldDataKey::ReserveBalance(asset.clone());
        let current: i128 = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_default();
        assert!(current >= amount, "InsufficientReserve");
        let new_balance = current - amount;
        self.env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    // --- pause / unpause -----------------------------------------------

    /// Return `true` if distributions are globally paused.
    pub fn is_paused(&self) -> bool {
        self.env
            .storage()
            .persistent()
            .get(&YieldDataKey::DistributionsPaused)
            .unwrap_or(false)
    }

    /// Set the global pause flag. Admin-only caller verified in lib.rs.
    pub fn set_paused(&self, paused: bool) {
        self.env
            .storage()
            .persistent()
            .set(&YieldDataKey::DistributionsPaused, &paused);
    }

    // --- distribution history ------------------------------------------

    /// Append a [`YieldDistributionRecord`] to the per-pair history log.
    pub fn record_distribution(&self, record: &YieldDistributionRecord) {
        let key = YieldDataKey::DistributionHistory(record.staker.clone(), record.asset.clone());
        let mut log: Vec<YieldDistributionRecord> = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env));
        log.push_back(record.clone());
        self.env.storage().persistent().set(&key, &log);
    }

    /// Full distribution history for a `(staker, asset)` pair, oldest first.
    pub fn distribution_history(
        &self,
        staker: &Address,
        asset: &Symbol,
    ) -> Vec<YieldDistributionRecord> {
        let key = YieldDataKey::DistributionHistory(staker.clone(), asset.clone());
        self.env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env))
    }

    /// Distribution history for a `(staker, asset)` pair filtered by time
    /// range `[from_ts, to_ts]` (inclusive).
    pub fn distribution_history_range(
        &self,
        staker: &Address,
        asset: &Symbol,
        from_ts: u64,
        to_ts: u64,
    ) -> Vec<YieldDistributionRecord> {
        let all = self.distribution_history(staker, asset);
        let mut filtered: Vec<YieldDistributionRecord> = Vec::new(self.env);
        for i in 0..all.len() {
            let entry = all.get(i).unwrap();
            if entry.timestamp >= from_ts && entry.timestamp <= to_ts {
                filtered.push_back(entry);
            }
        }
        filtered
    }

    /// Distribution history for a `(staker, asset)` pair filtered by type.
    pub fn distribution_history_by_type(
        &self,
        staker: &Address,
        asset: &Symbol,
        dist_type: DistributionType,
    ) -> Vec<YieldDistributionRecord> {
        let all = self.distribution_history(staker, asset);
        let mut filtered: Vec<YieldDistributionRecord> = Vec::new(self.env);
        for i in 0..all.len() {
            let entry = all.get(i).unwrap();
            if entry.distribution_type == dist_type {
                filtered.push_back(entry);
            }
        }
        filtered
    }

    /// Total yield claimed by a staker for an asset, across all distributions.
    pub fn total_yield_claimed(&self, staker: &Address, asset: &Symbol) -> i128 {
        let history = self.distribution_history(staker, asset);
        let mut total: i128 = 0;
        for i in 0..history.len() {
            let entry = history.get(i).unwrap();
            total = total
                .checked_add(entry.amount)
                .expect("total_yield_claimed overflow");
        }
        total
    }

    // --- partial claims ------------------------------------------------

    /// Claim a specific `amount` of yield from a position.
    ///
    /// The position is accrued to `now` first. If `amount` exceeds accrued
    /// yield, the full accrued amount is claimed. Returns the actual amount
    /// claimed.
    ///
    /// The reserve is checked: if a reserve exists for the asset and has
    /// insufficient balance, the claim is capped to the available reserve.
    pub fn claim_yield_partial(
        &self,
        staker: &Address,
        asset: &Symbol,
        amount: i128,
    ) -> Result<i128, MathError> {
        assert!(amount > 0, "ClaimAmountMustBePositive");
        let record = self
            .load_record(staker, asset)
            .ok_or(MathError::NegativeInput)?;
        let now = self.env.ledger().timestamp();
        let updated = self.accrue_to(&record, now)?;
        let available = updated.accrued_yield;

        // Cap to available accrued yield.
        let mut claimed = if amount > available {
            available
        } else {
            amount
        };

        // If reserve exists, cap to reserve balance.
        let reserve_key = YieldDataKey::ReserveBalance(asset.clone());
        let reserve: i128 = self
            .env
            .storage()
            .persistent()
            .get(&reserve_key)
            .unwrap_or(-1);
        if reserve >= 0 && claimed > reserve {
            claimed = reserve;
        }

        if claimed <= 0 {
            return Ok(0);
        }

        // Deduct from reserve if one exists.
        if reserve >= 0 {
            self.env
                .storage()
                .persistent()
                .set(&reserve_key, &(reserve - claimed));
        }

        // Finalize: deduct from accrued, record history.
        let mut finalized = updated.clone();
        finalized.accrued_yield -= claimed;
        self.append_history(
            staker,
            asset,
            YieldHistoryEntry {
                timestamp: now,
                period_seconds: 0,
                apr: finalized.apr,
                yield_earned: 0,
                cumulative_yield: finalized.accrued_yield,
                is_claim: true,
            },
        );
        self.store_record(&finalized);

        // Record the distribution.
        let remaining_reserve = self.reserve_balance(asset);
        self.record_distribution(&YieldDistributionRecord {
            staker: staker.clone(),
            asset: asset.clone(),
            amount: claimed,
            timestamp: now,
            distribution_type: DistributionType::Claim,
            accrued_at_claim: available,
            reserve_after: remaining_reserve,
        });

        Ok(claimed)
    }

    // --- batch claims ---------------------------------------------------

    /// Claim yield for multiple stakers on a single asset in one call.
    ///
    /// Gas optimization: accrual is done once per staker, and distribution
    /// records are written in a tight loop. Each staker claims all their
    /// accrued yield (full claim, not partial).
    ///
    /// Returns a `Vec` of `(staker, claimed_amount)` pairs.
    pub fn batch_claim(&self, stakers: &Vec<Address>, asset: &Symbol) -> Vec<(Address, i128)> {
        let now = self.env.ledger().timestamp();
        let mut results: Vec<(Address, i128)> = Vec::new(self.env);

        // Check if distributions are paused.
        if self.is_paused() {
            for i in 0..stakers.len() {
                let staker = stakers.get(i).unwrap();
                results.push_back((staker, 0));
            }
            return results;
        }

        // Reserve check: if reserve exists, compute total available.
        let reserve_key = YieldDataKey::ReserveBalance(asset.clone());
        let mut reserve_remaining: i128 = self
            .env
            .storage()
            .persistent()
            .get(&reserve_key)
            .unwrap_or(-1);
        let has_reserve = reserve_remaining >= 0;

        for i in 0..stakers.len() {
            let staker = stakers.get(i).unwrap();
            let record = match self.load_record(&staker, asset) {
                Some(r) => r,
                None => {
                    results.push_back((staker, 0));
                    continue;
                }
            };

            let updated = match self.accrue_to(&record, now) {
                Ok(r) => r,
                Err(_) => {
                    results.push_back((staker, 0));
                    continue;
                }
            };

            let available = updated.accrued_yield;
            if available <= 0 {
                results.push_back((staker, 0));
                continue;
            }

            let mut claimed = available;

            // Cap to reserve if one exists.
            if has_reserve && claimed > reserve_remaining {
                claimed = reserve_remaining;
            }

            if claimed <= 0 {
                results.push_back((staker, 0));
                continue;
            }

            // Deduct from reserve.
            if has_reserve {
                reserve_remaining -= claimed;
            }

            // Finalize the claim.
            let mut finalized = updated.clone();
            finalized.accrued_yield -= claimed;
            self.append_history(
                &staker,
                asset,
                YieldHistoryEntry {
                    timestamp: now,
                    period_seconds: 0,
                    apr: finalized.apr,
                    yield_earned: 0,
                    cumulative_yield: finalized.accrued_yield,
                    is_claim: true,
                },
            );
            self.store_record(&finalized);

            // Record the distribution.
            self.record_distribution(&YieldDistributionRecord {
                staker: staker.clone(),
                asset: asset.clone(),
                amount: claimed,
                timestamp: now,
                distribution_type: DistributionType::BatchClaim,
                accrued_at_claim: available,
                reserve_after: reserve_remaining,
            });

            results.push_back((staker, claimed));
        }

        // Persist the updated reserve.
        if has_reserve {
            self.env
                .storage()
                .persistent()
                .set(&reserve_key, &reserve_remaining);
        }

        results
    }

    // --- enhanced distribution processing --------------------------------

    /// Process due distributions for a pair as of the current ledger time,
    /// with reserve-solvency checks.
    ///
    /// If distributions are paused, returns `0` without modifying state.
    /// If a reserve is configured and insufficient for a distribution, that
    /// distribution is skipped.
    ///
    /// Returns the total amount marked due. One-off schedules are marked
    /// `executed`; recurring schedules have their `due_ts` advanced by their
    /// interval and remain active.
    pub fn process_distribution(&self, staker: &Address, asset: &Symbol) -> i128 {
        let now = self.env.ledger().timestamp();

        // Block all distributions when paused.
        if self.is_paused() {
            return 0;
        }

        let key = YieldDataKey::Schedule(staker.clone(), asset.clone());
        let list: Vec<DistributionSchedule> = match self.env.storage().persistent().get(&key) {
            Some(l) => l,
            None => return 0,
        };

        let reserve_key = YieldDataKey::ReserveBalance(asset.clone());
        let mut reserve: i128 = self
            .env
            .storage()
            .persistent()
            .get(&reserve_key)
            .unwrap_or(-1);
        let has_reserve = reserve >= 0;

        let mut total: i128 = 0;
        let mut updated: Vec<DistributionSchedule> = Vec::new(self.env);
        for i in 0..list.len() {
            let mut s = list.get(i).unwrap();
            if !s.executed && s.due_ts <= now {
                // Reserve-solvency check.
                let can_pay = if has_reserve {
                    reserve >= s.amount
                } else {
                    true
                };

                if can_pay {
                    total += s.amount;
                    if has_reserve {
                        reserve -= s.amount;
                    }

                    // Record the distribution.
                    self.record_distribution(&YieldDistributionRecord {
                        staker: staker.clone(),
                        asset: asset.clone(),
                        amount: s.amount,
                        timestamp: now,
                        distribution_type: DistributionType::Scheduled,
                        accrued_at_claim: s.amount,
                        reserve_after: reserve,
                    });

                    if s.interval_seconds > 0 {
                        s.due_ts += s.interval_seconds;
                    } else {
                        s.executed = true;
                    }
                }
                // If reserve is insufficient, the schedule stays pending
                // (not executed, not advanced) so it can be retried later.
            }
            updated.push_back(s);
        }
        self.env.storage().persistent().set(&key, &updated);

        // Persist updated reserve.
        if has_reserve {
            self.env.storage().persistent().set(&reserve_key, &reserve);
        }

        total
    }
}
