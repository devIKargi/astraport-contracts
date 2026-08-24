//! Configurable alert thresholds and monitoring system for staking operations.
//!
//! This module provides a comprehensive alert framework that monitors balance
//! changes, yield performance, and upcoming unlock dates against user-defined
//! thresholds. Alerts are delivered via Soroban events and stored on-chain for
//! audit and acknowledgment.
//!
//! # Architecture
//!
//! - [`AlertSeverity`] — three-tier severity ladder: Info, Warning, Critical.
//! - [`AlertKind`] — four condition types: balance drop, yield underperformance,
//!   upcoming unlock, and free-form custom conditions.
//! - [`AlertThreshold`] — the configurable trigger definition for one condition.
//! - [`AlertConfig`] — a staker's full set of preferences (collection of thresholds).
//! - [`AlertEvent`] — the on-chain notification emitted when a condition fires.
//! - [`AlertHistoryEntry`] — immutable audit-trail record of a past alert.
//! - [`AlertMonitor`] — stateful engine that evaluates thresholds and fires alerts.
//!
//! # Storage keys
//!
//! All persistence is routed through [`AlertDataKey`], stored under
//! `env.storage().persistent()`.

use soroban_sdk::{contracttype, symbol_short, Address, Env, String, Symbol, Vec};

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// The urgency level of an alert.
///
/// Consumers (UI, notification services) can filter on severity to decide how
/// prominently to surface each alert.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertSeverity {
    /// Informational — no action required, worth noting.
    Info = 0,
    /// Warning — the user should be aware and may want to act.
    Warning = 1,
    /// Critical — immediate attention is recommended.
    Critical = 2,
}

// ---------------------------------------------------------------------------
// Alert kind
// ---------------------------------------------------------------------------

/// Discriminator for the condition an [`AlertThreshold`] monitors.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// Fires when the staked balance falls below a configured floor.
    BalanceDrop,
    /// Fires when the current APR drops below a configured minimum.
    YieldUnderperformance,
    /// Fires when an unlock timestamp is within a configured lookahead window.
    UpcomingUnlock,
    /// User-defined condition identified by a custom tag.
    Custom,
}

// ---------------------------------------------------------------------------
// AlertThreshold
// ---------------------------------------------------------------------------

/// A single configurable trigger definition.
///
/// `trigger_value` is interpreted according to `kind`:
/// - `BalanceDrop`           — floor in base units; alert when balance < value.
/// - `YieldUnderperformance` — minimum APR in fixed-point; alert when APR < value.
/// - `UpcomingUnlock`        — lookahead in seconds; alert when unlock_ts - now < value.
/// - `Custom`                — user-defined threshold; semantics set by `label`.
///
/// `enabled` lets a staker temporarily silence a threshold without deleting it.
#[contracttype]
#[derive(Debug, Clone)]
pub struct AlertThreshold {
    /// Which condition this threshold monitors.
    pub kind: AlertKind,
    /// The numeric trigger level (interpretation depends on `kind`).
    pub trigger_value: i128,
    /// Severity to assign when this threshold fires.
    pub severity: AlertSeverity,
    /// Human-readable label (max 32 bytes recommended).
    pub label: String,
    /// When `false` the threshold is skipped during evaluation.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// AlertConfig
// ---------------------------------------------------------------------------

/// A staker's complete alert preferences for one `(staker, asset)` pair.
///
/// The `thresholds` vector is ordered and evaluated left-to-right by
/// [`AlertMonitor::check`]. A staker may register up to
/// [`MAX_THRESHOLDS_PER_CONFIG`] thresholds per asset.
#[contracttype]
#[derive(Debug, Clone)]
pub struct AlertConfig {
    /// The staker these preferences belong to.
    pub staker: Address,
    /// The asset these preferences apply to.
    pub asset: Symbol,
    /// Ordered list of threshold definitions.
    pub thresholds: Vec<AlertThreshold>,
    /// When `false`, no alerts are evaluated for this staker/asset pair.
    pub alerts_enabled: bool,
}

/// Maximum number of thresholds a single [`AlertConfig`] may hold.
pub const MAX_THRESHOLDS_PER_CONFIG: u32 = 16;

// ---------------------------------------------------------------------------
// AlertEvent
// ---------------------------------------------------------------------------

/// On-chain event payload emitted whenever a threshold fires.
///
/// Published under `(symbol_short!("ALERT"), staker, asset)`. Subscribers
/// index on staker or severity to power notification pipelines.
#[contracttype]
#[derive(Debug, Clone)]
pub struct AlertEvent {
    /// The staker the alert concerns.
    pub staker: Address,
    /// The asset the alert concerns.
    pub asset: Symbol,
    /// Which condition type fired.
    pub kind: AlertKind,
    /// Severity of the fired alert.
    pub severity: AlertSeverity,
    /// Ledger timestamp at which the alert fired.
    pub fired_at: u64,
    /// The threshold value that was breached.
    pub threshold_value: i128,
    /// The actual observed value that triggered the breach.
    pub observed_value: i128,
    /// Label copied from the threshold definition for display.
    pub label: String,
}

// ---------------------------------------------------------------------------
// AlertHistoryEntry
// ---------------------------------------------------------------------------

/// Immutable audit-trail record of an alert that fired.
///
/// Appended to the per-`(staker, asset)` history log each time
/// [`AlertMonitor::check`] fires an alert. Acknowledgment is tracked here via
/// [`acknowledged`].
#[contracttype]
#[derive(Debug, Clone)]
pub struct AlertHistoryEntry {
    /// Sequential index within the pair's history (0-based).
    pub index: u32,
    /// Which condition type fired.
    pub kind: AlertKind,
    /// Severity at the time the alert fired.
    pub severity: AlertSeverity,
    /// Ledger timestamp the alert fired at.
    pub fired_at: u64,
    /// Threshold value that was set.
    pub threshold_value: i128,
    /// Observed value that breached the threshold.
    pub observed_value: i128,
    /// Label from the threshold definition.
    pub label: String,
    /// Whether a staker has acknowledged this alert.
    pub acknowledged: bool,
}

// ---------------------------------------------------------------------------
// AlertDataKey
// ---------------------------------------------------------------------------

/// Persistent-storage keys for the alert subsystem.
#[contracttype]
#[derive(Debug, Clone)]
pub enum AlertDataKey {
    /// The [`AlertConfig`] for a `(staker, asset)` pair.
    Config(Address, Symbol),
    /// The alert history log for a `(staker, asset)` pair.
    History(Address, Symbol),
}

// ---------------------------------------------------------------------------
// AlertMonitor
// ---------------------------------------------------------------------------

/// Stateful engine that evaluates alert thresholds and records fired alerts.
///
/// Like [`crate::engine::YieldEngine`], `AlertMonitor` is a thin `env`-scoped
/// facade — it holds no state beyond a borrowed [`Env`] and reads/writes
/// everything through persistent storage.
pub struct AlertMonitor<'a> {
    env: &'a Env,
}

impl<'a> AlertMonitor<'a> {
    /// Create a monitor bound to the given environment.
    pub fn new(env: &'a Env) -> Self {
        Self { env }
    }

    // -----------------------------------------------------------------------
    // Configuration management
    // -----------------------------------------------------------------------

    /// Store (create or fully replace) the alert configuration for a pair.
    ///
    /// Call this to initialise preferences or to overwrite all thresholds at
    /// once. For incremental changes, use [`Self::add_threshold`] and
    /// [`Self::remove_threshold`].
    ///
    /// Returns the stored config.
    pub fn set_config(&self, config: AlertConfig) -> AlertConfig {
        let key = AlertDataKey::Config(config.staker.clone(), config.asset.clone());
        self.env.storage().persistent().set(&key, &config);
        config
    }

    /// Retrieve the alert configuration for a pair, if any.
    pub fn get_config(&self, staker: &Address, asset: &Symbol) -> Option<AlertConfig> {
        let key = AlertDataKey::Config(staker.clone(), asset.clone());
        self.env.storage().persistent().get(&key)
    }

    /// Append a new threshold to an existing config.
    ///
    /// Panics if no config exists for the pair (call [`Self::set_config`] first)
    /// or if [`MAX_THRESHOLDS_PER_CONFIG`] would be exceeded.
    ///
    /// Returns the updated config.
    pub fn add_threshold(
        &self,
        staker: &Address,
        asset: &Symbol,
        threshold: AlertThreshold,
    ) -> AlertConfig {
        let mut config = self
            .get_config(staker, asset)
            .expect("no alert config for this pair");
        assert!(
            config.thresholds.len() < MAX_THRESHOLDS_PER_CONFIG,
            "threshold limit reached"
        );
        config.thresholds.push_back(threshold);
        self.set_config(config)
    }

    /// Remove the threshold at position `index` (0-based) in the ordered list.
    ///
    /// Panics if `index` is out of range. Returns the updated config.
    pub fn remove_threshold(&self, staker: &Address, asset: &Symbol, index: u32) -> AlertConfig {
        let mut config = self
            .get_config(staker, asset)
            .expect("no alert config for this pair");
        assert!(
            (index as usize) < config.thresholds.len() as usize,
            "threshold index out of range"
        );

        // Rebuild the vec without the element at `index`.
        let mut updated: Vec<AlertThreshold> = Vec::new(self.env);
        for i in 0..config.thresholds.len() {
            if i != index {
                updated.push_back(config.thresholds.get(i).unwrap());
            }
        }
        config.thresholds = updated;
        self.set_config(config)
    }

    /// Enable or disable all alerts for a pair without modifying thresholds.
    pub fn set_alerts_enabled(
        &self,
        staker: &Address,
        asset: &Symbol,
        enabled: bool,
    ) -> AlertConfig {
        let mut config = self
            .get_config(staker, asset)
            .expect("no alert config for this pair");
        config.alerts_enabled = enabled;
        self.set_config(config)
    }

    // -----------------------------------------------------------------------
    // Threshold evaluation
    // -----------------------------------------------------------------------

    /// Evaluate all enabled thresholds for a pair against live observations.
    ///
    /// For each threshold that fires:
    /// 1. An [`AlertEvent`] is published via `env.events()`.
    /// 2. An [`AlertHistoryEntry`] is appended to the persistent history log.
    ///
    /// `current_balance` — the staker's current staked balance in base units.
    /// `current_apr`     — the position's current APR in fixed-point.
    /// `unlock_ts`       — optional unlock timestamp in ledger seconds; pass `0`
    ///                     if no lock-up applies (the unlock threshold is skipped).
    ///
    /// Returns the number of alerts that fired.
    pub fn check(
        &self,
        staker: &Address,
        asset: &Symbol,
        current_balance: i128,
        current_apr: i128,
        unlock_ts: u64,
    ) -> u32 {
        let config = match self.get_config(staker, asset) {
            Some(c) => c,
            None => return 0,
        };
        if !config.alerts_enabled {
            return 0;
        }

        let now = self.env.ledger().timestamp();
        let mut fired: u32 = 0;

        for i in 0..config.thresholds.len() {
            let t = config.thresholds.get(i).unwrap();
            if !t.enabled {
                continue;
            }

            let observed =
                self.observed_value(&t.kind, current_balance, current_apr, unlock_ts, now);
            if self.threshold_breached(&t, observed, now, unlock_ts) {
                self.emit_and_record(staker, asset, &t, observed, now);
                fired += 1;
            }
        }

        fired
    }

    /// Derive the current observed value for a threshold kind.
    fn observed_value(
        &self,
        kind: &AlertKind,
        balance: i128,
        apr: i128,
        unlock_ts: u64,
        now: u64,
    ) -> i128 {
        match kind {
            AlertKind::BalanceDrop => balance,
            AlertKind::YieldUnderperformance => apr,
            AlertKind::UpcomingUnlock => {
                if unlock_ts == 0 || unlock_ts <= now {
                    0
                } else {
                    (unlock_ts - now) as i128
                }
            }
            AlertKind::Custom => 0, // custom alerts always rely on explicit trigger; fire only when observed == threshold
        }
    }

    /// Return `true` when a threshold condition is met for the observed value.
    fn threshold_breached(
        &self,
        threshold: &AlertThreshold,
        observed: i128,
        now: u64,
        unlock_ts: u64,
    ) -> bool {
        match threshold.kind {
            // Alert when balance drops below the floor.
            AlertKind::BalanceDrop => observed < threshold.trigger_value,
            // Alert when the APR falls below the configured minimum.
            AlertKind::YieldUnderperformance => observed < threshold.trigger_value,
            // Alert when the unlock is within the lookahead window (and non-zero).
            AlertKind::UpcomingUnlock => {
                if unlock_ts == 0 || unlock_ts <= now {
                    false
                } else {
                    observed < threshold.trigger_value
                }
            }
            // Custom: caller must set trigger_value == observed to fire.
            AlertKind::Custom => observed == threshold.trigger_value,
        }
    }

    /// Emit an event and append a history entry for a fired threshold.
    fn emit_and_record(
        &self,
        staker: &Address,
        asset: &Symbol,
        threshold: &AlertThreshold,
        observed: i128,
        now: u64,
    ) {
        // Build the event payload.
        let event = AlertEvent {
            staker: staker.clone(),
            asset: asset.clone(),
            kind: threshold.kind,
            severity: threshold.severity,
            fired_at: now,
            threshold_value: threshold.trigger_value,
            observed_value: observed,
            label: threshold.label.clone(),
        };

        // Publish via Soroban events.
        self.env.events().publish(
            (symbol_short!("ALERT"), staker.clone(), asset.clone()),
            event,
        );

        // Append to the persistent history log.
        let key = AlertDataKey::History(staker.clone(), asset.clone());
        let mut log: Vec<AlertHistoryEntry> = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env));

        let entry = AlertHistoryEntry {
            index: log.len(),
            kind: threshold.kind,
            severity: threshold.severity,
            fired_at: now,
            threshold_value: threshold.trigger_value,
            observed_value: observed,
            label: threshold.label.clone(),
            acknowledged: false,
        };

        log.push_back(entry);
        self.env.storage().persistent().set(&key, &log);
    }

    // -----------------------------------------------------------------------
    // History and acknowledgment
    // -----------------------------------------------------------------------

    /// Return the full alert history for a pair, oldest entry first.
    pub fn history(&self, staker: &Address, asset: &Symbol) -> Vec<AlertHistoryEntry> {
        let key = AlertDataKey::History(staker.clone(), asset.clone());
        self.env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env))
    }

    /// Acknowledge the alert at position `index` in the history log.
    ///
    /// Acknowledged alerts are still retained for audit purposes; only the
    /// `acknowledged` flag is flipped. Panics if `index` is out of range.
    pub fn acknowledge(&self, staker: &Address, asset: &Symbol, index: u32) {
        let key = AlertDataKey::History(staker.clone(), asset.clone());
        let log: Vec<AlertHistoryEntry> = self
            .env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(self.env));

        assert!(
            (index as usize) < log.len() as usize,
            "alert index out of range"
        );

        let mut entry = log.get(index).unwrap();
        entry.acknowledged = true;

        // Rebuild the vec with the updated entry.
        let mut updated: Vec<AlertHistoryEntry> = Vec::new(self.env);
        for i in 0..log.len() {
            if i == index {
                updated.push_back(entry.clone());
            } else {
                updated.push_back(log.get(i).unwrap());
            }
        }
        self.env.storage().persistent().set(&key, &updated);
    }

    /// Return only the unacknowledged alerts for a pair.
    pub fn pending_alerts(&self, staker: &Address, asset: &Symbol) -> Vec<AlertHistoryEntry> {
        let all = self.history(staker, asset);
        let mut pending: Vec<AlertHistoryEntry> = Vec::new(self.env);
        for i in 0..all.len() {
            let entry = all.get(i).unwrap();
            if !entry.acknowledged {
                pending.push_back(entry);
            }
        }
        pending
    }
}
