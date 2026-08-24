#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Bytes, Env, Map,
    Symbol, Vec, U256,
};

pub mod subscriptions;

#[cfg(test)]
mod test_subscriptions;

use subscriptions::{
    backoff_delay, DeliveryMode, DeliveryRecord, DeliveryStatus, EventFilter, ManagedSubscription,
    SubscriptionPreferences, SubscriptionStatus,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OK: Symbol = symbol_short!("OK");
const TRIG_ADD: Symbol = symbol_short!("TRIG_ADD");
const ANALYSIS: Symbol = symbol_short!("ANALYSIS");
const RECOMMEND: Symbol = symbol_short!("RECMD");
const TIMEOUT: Symbol = symbol_short!("TIMEOUT");

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the events contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyExists = 1,
    NotFound = 2,
    Unauthorized = 3,
    InvalidState = 4,
}

// ---------------------------------------------------------------------------
// Portfolio event types for the event emission / subscription system
// ---------------------------------------------------------------------------

/// Portfolio-specific event types emitted when portfolio state changes.
///
/// Subscribers can filter on these types when subscribing.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioEventType {
    /// Portfolio was rebalanced.
    Rebalanced = 0,
    /// A balance changed (stake, unstake, deposit, withdrawal).
    BalanceChanged = 1,
    /// The target allocation was updated.
    AllocationUpdated = 2,
    /// A configured threshold was breached.
    ThresholdBreached = 3,
    /// A trade was executed.
    TradeExecuted = 4,
    /// A price alert was triggered.
    PriceAlertTriggered = 5,
    /// Free-form custom event.
    Custom = 99,
}

// ---------------------------------------------------------------------------
// Event record
// ---------------------------------------------------------------------------

/// An immutable event record emitted when portfolio state changes.
///
/// Stored in a per-portfolio vector and also published as a Soroban event so
/// off-chain indexers can pick it up. The `event_id` is a monotonically
/// increasing identifier unique across all portfolios.
#[contracttype]
#[derive(Debug, Clone)]
pub struct Event {
    /// Globally unique, monotonically increasing identifier.
    pub event_id: u64,
    /// The portfolio this event concerns.
    pub portfolio_id: Symbol,
    /// The kind of change that occurred.
    pub event_type: PortfolioEventType,
    /// Ledger timestamp when the event was emitted.
    pub timestamp: u64,
    /// Arbitrary key-value details attached to the event.
    pub details: Map<Symbol, Bytes>,
    /// Opaque metadata blob (empty if none).
    pub metadata: Bytes,
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// A subscription registration.
///
/// Each subscription records which portfolio it watches, which event types it
/// cares about (empty = all), and the ordering index so that subscribers are
/// notified in the order they registered.
#[contracttype]
#[derive(Debug, Clone)]
pub struct Subscription {
    /// The subscriber (external contract or account).
    pub subscriber: Address,
    /// The portfolio this subscription watches.
    pub portfolio_id: Symbol,
    /// The event types the subscriber is interested in.
    /// An empty vector means "all event types".
    pub event_types: Vec<PortfolioEventType>,
    /// The ordering index assigned at subscription time (0-based).
    pub order_index: u32,
    /// Ledger timestamp when the subscription was created.
    pub subscribed_at: u64,
    /// Whether the subscription is currently active.
    pub is_active: bool,
}

// ---------------------------------------------------------------------------
// Supporting types (AI trigger framework – kept from the original contract)
// ---------------------------------------------------------------------------

/// Supported event types that can trigger AI analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum EventType {
    PortfolioRebalance = 0,
    TradeExecuted = 1,
    PriceThresholdCrossed = 2,
    VolatilitySpike = 3,
    LiquidityChange = 4,
    CustomEvent = 99,
}

impl From<u32> for EventType {
    fn from(value: u32) -> Self {
        match value {
            0 => EventType::PortfolioRebalance,
            1 => EventType::TradeExecuted,
            2 => EventType::PriceThresholdCrossed,
            3 => EventType::VolatilitySpike,
            4 => EventType::LiquidityChange,
            _ => EventType::CustomEvent,
        }
    }
}

/// Comparison operators for threshold conditions.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ComparisonOperator {
    GreaterThan = 0,
    LessThan = 1,
    EqualTo = 2,
    GreaterOrEqual = 3,
    LessOrEqual = 4,
}

impl From<u32> for ComparisonOperator {
    fn from(value: u32) -> Self {
        match value {
            0 => ComparisonOperator::GreaterThan,
            1 => ComparisonOperator::LessThan,
            2 => ComparisonOperator::EqualTo,
            3 => ComparisonOperator::GreaterOrEqual,
            4 => ComparisonOperator::LessOrEqual,
            _ => ComparisonOperator::GreaterThan,
        }
    }
}

/// Status of an analysis request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AnalysisStatus {
    Pending = 0,
    InProgress = 1,
    Completed = 2,
    Failed = 3,
    TimedOut = 4,
}

impl From<u32> for AnalysisStatus {
    fn from(value: u32) -> Self {
        match value {
            0 => AnalysisStatus::Pending,
            1 => AnalysisStatus::InProgress,
            2 => AnalysisStatus::Completed,
            3 => AnalysisStatus::Failed,
            4 => AnalysisStatus::TimedOut,
            _ => AnalysisStatus::Pending,
        }
    }
}

/// Recommendation action types from AI analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RecommendationType {
    Hold = 0,
    Buy = 1,
    Sell = 2,
    Rebalance = 3,
    Monitor = 4,
    NoAction = 5,
}

impl From<u32> for RecommendationType {
    fn from(value: u32) -> Self {
        match value {
            0 => RecommendationType::Hold,
            1 => RecommendationType::Buy,
            2 => RecommendationType::Sell,
            3 => RecommendationType::Rebalance,
            4 => RecommendationType::Monitor,
            _ => RecommendationType::NoAction,
        }
    }
}

/// AITrigger defines when AI analysis should be invoked.
#[contracttype]
#[derive(Debug, Clone)]
pub struct AITrigger {
    pub trigger_id: Symbol,
    pub name: Symbol,
    pub event_types: Vec<u32>,
    pub has_threshold: bool,
    pub threshold: U256,
    pub has_operator: bool,
    pub operator: u32,
    pub ai_service_endpoint: Address,
    pub timeout: u64,
    pub is_active: bool,
    pub owner: Address,
}

/// Condition to evaluate against current values.
pub struct TriggerCondition {
    pub current_value: i128,
    pub threshold: i128,
    pub operator: ComparisonOperator,
}

/// TriggerEvaluator handles condition checking for triggers.
pub struct TriggerEvaluator;

impl TriggerEvaluator {
    pub fn evaluate(
        trigger: &AITrigger,
        event_type: EventType,
        current_value: Option<i128>,
    ) -> bool {
        let event_type_matches = trigger.event_types.contains(event_type as u32);
        if !event_type_matches {
            return false;
        }
        if !trigger.has_threshold || !trigger.has_operator {
            return true;
        }
        let Some(value) = current_value else {
            return false;
        };
        let condition = TriggerCondition {
            current_value: value,
            threshold: trigger
                .threshold
                .to_u128()
                .map_or(i128::MAX, |v| i128::try_from(v).unwrap_or(i128::MAX)),
            operator: ComparisonOperator::from(trigger.operator),
        };
        Self::evaluate_condition(&condition)
    }

    fn evaluate_condition(condition: &TriggerCondition) -> bool {
        match condition.operator {
            ComparisonOperator::GreaterThan => condition.current_value > condition.threshold,
            ComparisonOperator::LessThan => condition.current_value < condition.threshold,
            ComparisonOperator::EqualTo => condition.current_value == condition.threshold,
            ComparisonOperator::GreaterOrEqual => condition.current_value >= condition.threshold,
            ComparisonOperator::LessOrEqual => condition.current_value <= condition.threshold,
        }
    }
}

/// AnalysisResult stores the output from AI service analysis.
#[contracttype]
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub analysis_id: u64,
    pub trigger_id: Symbol,
    pub portfolio_id: Symbol,
    pub timestamp: u64,
    pub latency_ms: u64,
    pub status: u32,
    pub raw_output: Bytes,
    pub error_message: Symbol,
}

/// Recommendation generated from AI analysis output.
#[contracttype]
#[derive(Debug, Clone)]
pub struct Recommendation {
    pub recommendation_id: u64,
    pub analysis_id: u64,
    pub portfolio_id: Symbol,
    pub action_type: u32,
    pub asset: Symbol,
    pub has_amount: bool,
    pub amount: U256,
    pub confidence_score: u32,
    pub timestamp: u64,
    pub accepted: Option<bool>,
}

/// AnalysisMetrics tracks performance metrics for AI analysis.
#[contracttype]
#[derive(Debug, Clone, Default)]
pub struct AnalysisMetrics {
    pub total_analyses: u64,
    pub successful_analyses: u64,
    pub failed_analyses: u64,
    pub timed_out_analyses: u64,
    pub average_latency_ms: u64,
    pub recommendations_accepted: u64,
    pub recommendations_rejected: u64,
}

// ---------------------------------------------------------------------------
// AI service client (stub)
// ---------------------------------------------------------------------------

pub trait AIServiceClient {
    fn submit_analysis(
        env: &Env,
        trigger: &AITrigger,
        portfolio_id: Symbol,
        event_data: Bytes,
    ) -> Result<u64, Symbol>;

    fn check_analysis_status(env: &Env, analysis_id: u64) -> Result<AnalysisStatus, Symbol>;
}

pub struct SorobanAIServiceClient;

impl AIServiceClient for SorobanAIServiceClient {
    fn submit_analysis(
        env: &Env,
        trigger: &AITrigger,
        portfolio_id: Symbol,
        _event_data: Bytes,
    ) -> Result<u64, Symbol> {
        let analysis_id = next_id(env);
        env.events().publish(
            (
                symbol_short!("ANAL_SUB"),
                portfolio_id,
                trigger.trigger_id.clone(),
            ),
            analysis_id,
        );
        Ok(analysis_id)
    }

    fn check_analysis_status(env: &Env, analysis_id: u64) -> Result<AnalysisStatus, Symbol> {
        let key = storage_keys::analysis_status(analysis_id);
        if env.storage().persistent().has(&key) {
            let status: u32 = env.storage().persistent().get(&key).unwrap();
            Ok(AnalysisStatus::from(status))
        } else {
            Err(symbol_short!("NOT_FOUND"))
        }
    }
}

pub struct RecommendationEngine;

impl RecommendationEngine {
    pub fn generate_recommendation(
        env: &Env,
        analysis: &AnalysisResult,
        ai_output: &Map<Symbol, u32>,
    ) -> Result<Recommendation, Symbol> {
        if analysis.status != AnalysisStatus::Completed as u32 {
            return Err(symbol_short!("BAD_STATE"));
        }
        let action_type = if let Some(action) = ai_output.get(symbol_short!("action")) {
            match action {
                0 => RecommendationType::Hold as u32,
                1 => RecommendationType::Buy as u32,
                2 => RecommendationType::Sell as u32,
                3 => RecommendationType::Rebalance as u32,
                4 => RecommendationType::Monitor as u32,
                _ => RecommendationType::NoAction as u32,
            }
        } else {
            RecommendationType::NoAction as u32
        };
        let confidence = ai_output.get(symbol_short!("conf")).unwrap_or(0);
        let timestamp = env.ledger().timestamp();
        Ok(Recommendation {
            recommendation_id: next_id(env),
            analysis_id: analysis.analysis_id,
            portfolio_id: analysis.portfolio_id.clone(),
            action_type,
            asset: symbol_short!(""),
            has_amount: false,
            amount: U256::from_u32(env, 0),
            confidence_score: confidence,
            timestamp,
            accepted: None,
        })
    }
}

// ---------------------------------------------------------------------------
// ID generator
// ---------------------------------------------------------------------------

fn next_id(env: &Env) -> u64 {
    let key = symbol_short!("next_id");
    let current: u64 = env.storage().persistent().get(&key).unwrap_or(0);
    let next = current + 1;
    env.storage().persistent().set(&key, &next);
    next
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

mod storage_keys {
    use super::*;

    pub fn triggers() -> Symbol {
        symbol_short!("triggers")
    }
    pub fn analyses() -> Symbol {
        symbol_short!("analyses")
    }
    pub fn recommendations() -> Symbol {
        symbol_short!("recs")
    }
    pub fn metrics() -> Symbol {
        symbol_short!("metrics")
    }
    #[allow(dead_code)]
    pub fn subscribers(portfolio_id: Symbol) -> (Symbol, Symbol) {
        (symbol_short!("subs"), portfolio_id)
    }
    #[allow(dead_code)]
    pub fn analysis_status(analysis_id: u64) -> (Symbol, u64) {
        (symbol_short!("status"), analysis_id)
    }
    pub fn subscriptions(portfolio_id: Symbol) -> (Symbol, Symbol) {
        (symbol_short!("sub_list"), portfolio_id)
    }
    pub fn event_history(portfolio_id: Symbol) -> (Symbol, Symbol) {
        (symbol_short!("ev_hist"), portfolio_id)
    }
    // Advanced subscription system (issue #7)
    pub fn managed_subs(portfolio_id: Symbol) -> (Symbol, Symbol) {
        (symbol_short!("mgd_subs"), portfolio_id)
    }
    pub fn managed_sub(subscription_id: u64) -> (Symbol, u64) {
        (symbol_short!("mgd_sub"), subscription_id)
    }
    pub fn deliveries(subscriber: Address) -> (Symbol, Address) {
        (symbol_short!("dlv"), subscriber)
    }
    pub fn pending_batch(subscriber: Address) -> (Symbol, Address) {
        (symbol_short!("batch"), subscriber)
    }
    pub fn delivery_metrics() -> Symbol {
        symbol_short!("dlv_mtc")
    }
    pub fn event_severity(event_id: u64) -> (Symbol, u64) {
        (symbol_short!("sev"), event_id)
    }
    pub fn sub_counter() -> Symbol {
        symbol_short!("sub_cnt")
    }
}

// ---------------------------------------------------------------------------
// Event emission helpers
// ---------------------------------------------------------------------------

fn matches_filter(sub: &Subscription, event_type: &PortfolioEventType) -> bool {
    if sub.event_types.is_empty() {
        return true;
    }
    for i in 0..sub.event_types.len() {
        if sub.event_types.get(i).unwrap() == *event_type {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct EventsContract;

#[contractimpl]
impl EventsContract {
    pub fn initialize(env: Env) -> Symbol {
        if !env.storage().persistent().has(&storage_keys::metrics()) {
            let metrics = AnalysisMetrics::default();
            env.storage()
                .persistent()
                .set(&storage_keys::metrics(), &metrics);
        }
        OK
    }

    pub fn add_trigger(env: Env, trigger: AITrigger) -> Result<Symbol, Error> {
        trigger.owner.require_auth();
        let mut triggers: Map<Symbol, AITrigger> = env
            .storage()
            .persistent()
            .get(&storage_keys::triggers())
            .unwrap_or_else(|| Map::new(&env));
        let trigger_id = trigger.trigger_id.clone();
        if triggers.contains_key(trigger_id.clone()) {
            return Err(Error::AlreadyExists);
        }
        triggers.set(trigger_id.clone(), trigger);
        env.storage()
            .persistent()
            .set(&storage_keys::triggers(), &triggers);
        env.events().publish((TRIG_ADD,), trigger_id);
        Ok(OK)
    }

    pub fn remove_trigger(env: Env, trigger_id: Symbol, owner: Address) -> Result<Symbol, Error> {
        owner.require_auth();
        let mut triggers: Map<Symbol, AITrigger> = env
            .storage()
            .persistent()
            .get(&storage_keys::triggers())
            .ok_or(Error::NotFound)?;
        let trigger = triggers.get(trigger_id.clone()).ok_or(Error::NotFound)?;
        if trigger.owner != owner {
            return Err(Error::Unauthorized);
        }
        triggers.remove(trigger_id.clone());
        env.storage()
            .persistent()
            .set(&storage_keys::triggers(), &triggers);
        env.events()
            .publish((symbol_short!("TRIG_RMV"),), trigger_id);
        Ok(OK)
    }

    pub fn process_event(
        env: Env,
        portfolio_id: Symbol,
        event_type: u32,
        event_data: Bytes,
        current_value: Option<U256>,
    ) -> Result<Vec<u64>, Error> {
        let event = EventType::from(event_type);
        let triggers: Map<Symbol, AITrigger> = env
            .storage()
            .persistent()
            .get(&storage_keys::triggers())
            .unwrap_or_else(|| Map::new(&env));
        let mut triggered_analyses = Vec::new(&env);
        let mut metrics: AnalysisMetrics = env
            .storage()
            .persistent()
            .get(&storage_keys::metrics())
            .unwrap_or_default();

        for (trigger_id, trigger) in triggers.iter() {
            if !trigger.is_active {
                continue;
            }
            let current_value_i128 = current_value
                .as_ref()
                .and_then(|v| v.to_u128())
                .and_then(|v| i128::try_from(v).ok());
            if TriggerEvaluator::evaluate(&trigger, event, current_value_i128) {
                match SorobanAIServiceClient::submit_analysis(
                    &env,
                    &trigger,
                    portfolio_id.clone(),
                    event_data.clone(),
                ) {
                    Ok(analysis_id) => {
                        let mut analyses: Map<u64, AnalysisResult> = env
                            .storage()
                            .persistent()
                            .get(&storage_keys::analyses())
                            .unwrap_or_else(|| Map::new(&env));
                        let timestamp = env.ledger().timestamp();
                        let analysis = AnalysisResult {
                            analysis_id,
                            trigger_id: trigger_id.clone(),
                            portfolio_id: portfolio_id.clone(),
                            timestamp,
                            latency_ms: 0,
                            status: AnalysisStatus::Pending as u32,
                            raw_output: Bytes::new(&env),
                            error_message: symbol_short!(""),
                        };
                        analyses.set(analysis_id, analysis);
                        env.storage()
                            .persistent()
                            .set(&storage_keys::analyses(), &analyses);
                        env.storage().persistent().set(
                            &storage_keys::analysis_status(analysis_id),
                            &(AnalysisStatus::Pending as u32),
                        );
                        metrics.total_analyses += 1;
                        triggered_analyses.push_back(analysis_id);
                        env.events().publish(
                            (ANALYSIS, portfolio_id.clone(), trigger_id.clone()),
                            analysis_id,
                        );
                    }
                    Err(e) => {
                        env.events()
                            .publish((symbol_short!("ERROR"), trigger_id.clone()), e);
                    }
                }
            }
        }
        env.storage()
            .persistent()
            .set(&storage_keys::metrics(), &metrics);
        Ok(triggered_analyses)
    }

    pub fn update_analysis_status(
        env: Env,
        analysis_id: u64,
        status: u32,
        latency_ms: Option<u64>,
        raw_output: Option<Bytes>,
        error: Option<Symbol>,
    ) -> Result<Symbol, Error> {
        let mut analyses: Map<u64, AnalysisResult> = env
            .storage()
            .persistent()
            .get(&storage_keys::analyses())
            .ok_or(Error::NotFound)?;
        let mut analysis = analyses.get(analysis_id).ok_or(Error::NotFound)?;
        let new_status = AnalysisStatus::from(status);
        analysis.status = status;
        if let Some(latency) = latency_ms {
            analysis.latency_ms = latency;
        }
        if let Some(output) = raw_output {
            analysis.raw_output = output;
        }
        analysis.error_message = error.unwrap_or(symbol_short!(""));

        analyses.set(analysis_id, analysis.clone());
        env.storage()
            .persistent()
            .set(&storage_keys::analyses(), &analyses);
        env.storage()
            .persistent()
            .set(&storage_keys::analysis_status(analysis_id), &status);

        let mut metrics: AnalysisMetrics = env
            .storage()
            .persistent()
            .get(&storage_keys::metrics())
            .unwrap_or_default();
        match new_status {
            AnalysisStatus::Completed => {
                metrics.successful_analyses += 1;
                if analysis.latency_ms > 0 && metrics.successful_analyses > 1 {
                    metrics.average_latency_ms = (metrics.average_latency_ms
                        * (metrics.successful_analyses - 1)
                        + analysis.latency_ms)
                        / metrics.successful_analyses;
                } else if analysis.latency_ms > 0 {
                    metrics.average_latency_ms = analysis.latency_ms;
                }
            }
            AnalysisStatus::Failed => metrics.failed_analyses += 1,
            AnalysisStatus::TimedOut => metrics.timed_out_analyses += 1,
            _ => {}
        }
        env.storage()
            .persistent()
            .set(&storage_keys::metrics(), &metrics);

        if new_status == AnalysisStatus::Completed {
            let mut ai_output: Map<Symbol, u32> = Map::new(&env);
            if !analysis.raw_output.is_empty() {
                ai_output.set(
                    symbol_short!("action"),
                    analysis.raw_output.get(0).unwrap_or(0) as u32,
                );
                ai_output.set(symbol_short!("conf"), 85);
            }
            if let Ok(recommendation) =
                RecommendationEngine::generate_recommendation(&env, &analysis, &ai_output)
            {
                let mut recommendations: Map<u64, Recommendation> = env
                    .storage()
                    .persistent()
                    .get(&storage_keys::recommendations())
                    .unwrap_or_else(|| Map::new(&env));
                let rec_id = recommendation.recommendation_id;
                recommendations.set(rec_id, recommendation);
                env.storage()
                    .persistent()
                    .set(&storage_keys::recommendations(), &recommendations);
                env.events()
                    .publish((RECOMMEND, analysis.portfolio_id, analysis_id), rec_id);
            }
        }
        Ok(OK)
    }

    pub fn process_timeout(env: Env, analysis_id: u64) -> Result<Symbol, Error> {
        let mut analyses: Map<u64, AnalysisResult> = env
            .storage()
            .persistent()
            .get(&storage_keys::analyses())
            .ok_or(Error::NotFound)?;
        let mut analysis = analyses.get(analysis_id).ok_or(Error::NotFound)?;
        if analysis.status != AnalysisStatus::Pending as u32
            && analysis.status != AnalysisStatus::InProgress as u32
        {
            return Err(Error::InvalidState);
        }
        analysis.status = AnalysisStatus::TimedOut as u32;
        analysis.error_message = TIMEOUT;
        analyses.set(analysis_id, analysis);
        env.storage()
            .persistent()
            .set(&storage_keys::analyses(), &analyses);
        env.storage().persistent().set(
            &storage_keys::analysis_status(analysis_id),
            &(AnalysisStatus::TimedOut as u32),
        );
        let mut metrics: AnalysisMetrics = env
            .storage()
            .persistent()
            .get(&storage_keys::metrics())
            .unwrap_or_default();
        metrics.timed_out_analyses += 1;
        env.storage()
            .persistent()
            .set(&storage_keys::metrics(), &metrics);
        env.events().publish((TIMEOUT,), analysis_id);
        Ok(OK)
    }

    pub fn process_recommendation_feedback(
        env: Env,
        recommendation_id: u64,
        accepted: bool,
        responder: Address,
    ) -> Result<Symbol, Error> {
        responder.require_auth();
        let mut recommendations: Map<u64, Recommendation> = env
            .storage()
            .persistent()
            .get(&storage_keys::recommendations())
            .ok_or(Error::NotFound)?;
        let mut rec = recommendations
            .get(recommendation_id)
            .ok_or(Error::NotFound)?;
        rec.accepted = Some(accepted);
        recommendations.set(recommendation_id, rec);
        env.storage()
            .persistent()
            .set(&storage_keys::recommendations(), &recommendations);
        let mut metrics: AnalysisMetrics = env
            .storage()
            .persistent()
            .get(&storage_keys::metrics())
            .unwrap_or_default();
        if accepted {
            metrics.recommendations_accepted += 1;
        } else {
            metrics.recommendations_rejected += 1;
        }
        env.storage()
            .persistent()
            .set(&storage_keys::metrics(), &metrics);
        Ok(OK)
    }

    pub fn get_portfolio_analyses(env: Env, portfolio_id: Symbol) -> Vec<AnalysisResult> {
        let analyses: Map<u64, AnalysisResult> = env
            .storage()
            .persistent()
            .get(&storage_keys::analyses())
            .unwrap_or_else(|| Map::new(&env));
        let mut results = Vec::new(&env);
        for (_, analysis) in analyses.iter() {
            if analysis.portfolio_id == portfolio_id {
                results.push_back(analysis);
            }
        }
        results
    }

    pub fn get_portfolio_recommendations(env: Env, portfolio_id: Symbol) -> Vec<Recommendation> {
        let recommendations: Map<u64, Recommendation> = env
            .storage()
            .persistent()
            .get(&storage_keys::recommendations())
            .unwrap_or_else(|| Map::new(&env));
        let mut results = Vec::new(&env);
        for (_, rec) in recommendations.iter() {
            if rec.portfolio_id == portfolio_id {
                results.push_back(rec);
            }
        }
        results
    }

    pub fn get_metrics(env: Env) -> AnalysisMetrics {
        env.storage()
            .persistent()
            .get(&storage_keys::metrics())
            .unwrap_or_default()
    }

    pub fn get_all_triggers(env: Env) -> Vec<AITrigger> {
        let triggers: Map<Symbol, AITrigger> = env
            .storage()
            .persistent()
            .get(&storage_keys::triggers())
            .unwrap_or_else(|| Map::new(&env));
        let mut results = Vec::new(&env);
        for (_, trigger) in triggers.iter() {
            results.push_back(trigger);
        }
        results
    }

    // ===================================================================
    // Event Emission & Subscription System
    // ===================================================================

    /// Subscribe to portfolio events with optional type filtering.
    pub fn subscribe(
        env: Env,
        portfolio_id: Symbol,
        subscriber: Address,
        event_types: Vec<PortfolioEventType>,
    ) -> Result<Symbol, Error> {
        subscriber.require_auth();
        let subs_key = storage_keys::subscriptions(portfolio_id.clone());
        let mut subscriptions: Vec<Subscription> = env
            .storage()
            .persistent()
            .get(&subs_key)
            .unwrap_or_else(|| Vec::new(&env));
        for i in 0..subscriptions.len() {
            let existing = subscriptions.get(i).unwrap();
            if existing.subscriber == subscriber && existing.is_active {
                return Ok(OK);
            }
        }
        let order_index = subscriptions.len();
        let sub = Subscription {
            subscriber: subscriber.clone(),
            portfolio_id: portfolio_id.clone(),
            event_types,
            order_index,
            subscribed_at: env.ledger().timestamp(),
            is_active: true,
        };
        subscriptions.push_back(sub);
        env.storage().persistent().set(&subs_key, &subscriptions);
        env.events().publish(
            (symbol_short!("SUB_ADD"), portfolio_id, subscriber),
            order_index,
        );
        Ok(OK)
    }

    /// Unsubscribe from portfolio events.
    pub fn unsubscribe(
        env: Env,
        portfolio_id: Symbol,
        subscriber: Address,
    ) -> Result<Symbol, Error> {
        subscriber.require_auth();
        let subs_key = storage_keys::subscriptions(portfolio_id.clone());
        let subscriptions: Vec<Subscription> = env
            .storage()
            .persistent()
            .get(&subs_key)
            .ok_or(Error::NotFound)?;
        let mut found = false;
        let mut updated: Vec<Subscription> = Vec::new(&env);
        for i in 0..subscriptions.len() {
            let mut sub = subscriptions.get(i).unwrap();
            if sub.subscriber == subscriber && sub.is_active {
                sub.is_active = false;
                found = true;
            }
            updated.push_back(sub);
        }
        if !found {
            return Err(Error::NotFound);
        }
        env.storage().persistent().set(&subs_key, &updated);
        env.events()
            .publish((symbol_short!("SUB_RMV"), portfolio_id, subscriber), 0u32);
        Ok(OK)
    }

    /// Emit a portfolio event and notify all matching active subscribers.
    pub fn emit_event(
        env: Env,
        portfolio_id: Symbol,
        event_type: PortfolioEventType,
        details: Map<Symbol, Bytes>,
        metadata: Bytes,
    ) -> Result<Event, Error> {
        let event_id = next_id(&env);
        let timestamp = env.ledger().timestamp();
        let event = Event {
            event_id,
            portfolio_id: portfolio_id.clone(),
            event_type,
            timestamp,
            details: details.clone(),
            metadata,
        };

        let hist_key = storage_keys::event_history(portfolio_id.clone());
        let mut history: Vec<Event> = env
            .storage()
            .persistent()
            .get(&hist_key)
            .unwrap_or_else(|| Vec::new(&env));
        history.push_back(event.clone());
        env.storage().persistent().set(&hist_key, &history);

        env.events().publish(
            (symbol_short!("PF_EVENT"), portfolio_id.clone(), event_type),
            event.clone(),
        );

        let subs_key = storage_keys::subscriptions(portfolio_id.clone());
        let subscriptions: Vec<Subscription> = env
            .storage()
            .persistent()
            .get(&subs_key)
            .unwrap_or_else(|| Vec::new(&env));
        for i in 0..subscriptions.len() {
            let sub = subscriptions.get(i).unwrap();
            if sub.is_active && matches_filter(&sub, &event_type) {
                env.events().publish(
                    (
                        symbol_short!("NOTIFY"),
                        sub.subscriber.clone(),
                        portfolio_id.clone(),
                    ),
                    event.event_id,
                );
            }
        }
        Ok(event)
    }

    /// Emit a portfolio event without metadata (convenience).
    pub fn emit_event_simple(
        env: Env,
        portfolio_id: Symbol,
        event_type: PortfolioEventType,
        details: Map<Symbol, Bytes>,
    ) -> Result<Event, Error> {
        Self::emit_event(
            env.clone(),
            portfolio_id,
            event_type,
            details,
            Bytes::new(&env),
        )
    }

    /// Subscribe to *all* events (legacy convenience).
    pub fn subscribe_all(
        env: Env,
        portfolio_id: Symbol,
        subscriber: Address,
    ) -> Result<Symbol, Error> {
        Self::subscribe(env.clone(), portfolio_id, subscriber, Vec::new(&env))
    }

    /// Unsubscribe from *all* events (legacy convenience).
    pub fn unsubscribe_all(
        env: Env,
        portfolio_id: Symbol,
        subscriber: Address,
    ) -> Result<Symbol, Error> {
        Self::unsubscribe(env, portfolio_id, subscriber)
    }

    // --- Query functions ---

    pub fn get_subscriptions(env: Env, portfolio_id: Symbol) -> Vec<Subscription> {
        let subs_key = storage_keys::subscriptions(portfolio_id);
        env.storage()
            .persistent()
            .get(&subs_key)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn get_active_subscriptions(env: Env, portfolio_id: Symbol) -> Vec<Subscription> {
        let all = Self::get_subscriptions(env.clone(), portfolio_id);
        let mut active = Vec::new(&env);
        for i in 0..all.len() {
            let sub = all.get(i).unwrap();
            if sub.is_active {
                active.push_back(sub);
            }
        }
        active
    }

    pub fn get_event_history(env: Env, portfolio_id: Symbol) -> Vec<Event> {
        let hist_key = storage_keys::event_history(portfolio_id);
        env.storage()
            .persistent()
            .get(&hist_key)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn get_events_by_type(
        env: Env,
        portfolio_id: Symbol,
        event_type: PortfolioEventType,
    ) -> Vec<Event> {
        let all = Self::get_event_history(env.clone(), portfolio_id.clone());
        let mut filtered = Vec::new(&env);
        for i in 0..all.len() {
            let event = all.get(i).unwrap();
            if event.event_type == event_type {
                filtered.push_back(event);
            }
        }
        filtered
    }

    pub fn get_events_by_time_range(
        env: Env,
        portfolio_id: Symbol,
        from_timestamp: u64,
        to_timestamp: u64,
    ) -> Vec<Event> {
        let all = Self::get_event_history(env.clone(), portfolio_id.clone());
        let mut filtered = Vec::new(&env);
        for i in 0..all.len() {
            let event = all.get(i).unwrap();
            if event.timestamp >= from_timestamp && event.timestamp <= to_timestamp {
                filtered.push_back(event);
            }
        }
        filtered
    }

    pub fn get_events_filtered(
        env: Env,
        portfolio_id: Symbol,
        event_type: PortfolioEventType,
        from_timestamp: u64,
        to_timestamp: u64,
    ) -> Vec<Event> {
        let all = Self::get_event_history(env.clone(), portfolio_id.clone());
        let mut filtered = Vec::new(&env);
        for i in 0..all.len() {
            let event = all.get(i).unwrap();
            if event.event_type == event_type
                && event.timestamp >= from_timestamp
                && event.timestamp <= to_timestamp
            {
                filtered.push_back(event);
            }
        }
        filtered
    }

    pub fn get_event_count(env: Env, portfolio_id: Symbol) -> u32 {
        let hist_key = storage_keys::event_history(portfolio_id);
        let history: Vec<Event> = env
            .storage()
            .persistent()
            .get(&hist_key)
            .unwrap_or_else(|| Vec::new(&env));
        history.len()
    }

    // -----------------------------------------------------------------------
    // Advanced subscription system (issue #7)
    // -----------------------------------------------------------------------

    /// Create a managed subscription with filtering and delivery preferences.
    ///
    /// Returns the new subscription id.
    pub fn create_subscription(
        env: Env,
        portfolio_id: Symbol,
        subscriber: Address,
        filter: EventFilter,
        prefs: SubscriptionPreferences,
    ) -> Result<u64, Error> {
        subscriber.require_auth();

        let id = {
            let key = storage_keys::sub_counter();
            let current: u64 = env.storage().persistent().get(&key).unwrap_or(0);
            let next = current + 1;
            env.storage().persistent().set(&key, &next);
            next
        };

        let sub = ManagedSubscription {
            id,
            subscriber,
            portfolio_id: portfolio_id.clone(),
            filter,
            prefs,
            created_at: env.ledger().timestamp(),
            last_event_received_at: None,
            status: SubscriptionStatus::Active,
            total_delivered: 0,
            total_failed: 0,
        };

        let list_key = storage_keys::managed_subs(portfolio_id.clone());
        let mut list: Vec<u64> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        list.push_back(id);
        env.storage().persistent().set(&list_key, &list);
        env.storage()
            .persistent()
            .set(&storage_keys::managed_sub(id), &sub);

        env.events()
            .publish((symbol_short!("MGD_SUB"), portfolio_id), id);
        Ok(id)
    }

    /// Update delivery preferences for a managed subscription. Owner only.
    pub fn update_subscription_prefs(
        env: Env,
        subscription_id: u64,
        prefs: SubscriptionPreferences,
    ) -> Result<Symbol, Error> {
        let mut sub =
            Self::get_managed_subscription(env.clone(), subscription_id).ok_or(Error::NotFound)?;
        sub.subscriber.require_auth();
        if sub.status == SubscriptionStatus::Cancelled {
            return Err(Error::InvalidState);
        }
        sub.prefs = prefs;
        env.storage()
            .persistent()
            .set(&storage_keys::managed_sub(subscription_id), &sub);
        Ok(OK)
    }

    /// Pause event delivery for a managed subscription. Owner only.
    pub fn pause_subscription(env: Env, subscription_id: u64) -> Result<Symbol, Error> {
        Self::set_subscription_status(env, subscription_id, SubscriptionStatus::Paused)
    }

    /// Resume a paused managed subscription. Owner only.
    pub fn resume_subscription(env: Env, subscription_id: u64) -> Result<Symbol, Error> {
        Self::set_subscription_status(env, subscription_id, SubscriptionStatus::Active)
    }

    /// Cancel a managed subscription permanently. Owner only.
    pub fn cancel_subscription(env: Env, subscription_id: u64) -> Result<Symbol, Error> {
        Self::set_subscription_status(env, subscription_id, SubscriptionStatus::Cancelled)
    }

    fn set_subscription_status(
        env: Env,
        subscription_id: u64,
        status: SubscriptionStatus,
    ) -> Result<Symbol, Error> {
        let mut sub =
            Self::get_managed_subscription(env.clone(), subscription_id).ok_or(Error::NotFound)?;
        sub.subscriber.require_auth();
        if sub.status == SubscriptionStatus::Cancelled && status != SubscriptionStatus::Cancelled {
            return Err(Error::InvalidState);
        }
        sub.status = status;
        env.storage()
            .persistent()
            .set(&storage_keys::managed_sub(subscription_id), &sub);
        Ok(OK)
    }

    /// Fetch one managed subscription by id.
    pub fn get_managed_subscription(env: Env, subscription_id: u64) -> Option<ManagedSubscription> {
        env.storage()
            .persistent()
            .get(&storage_keys::managed_sub(subscription_id))
    }

    /// List all managed subscription ids for a portfolio.
    pub fn list_managed_subscriptions(env: Env, portfolio_id: Symbol) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&storage_keys::managed_subs(portfolio_id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Emit an event with a severity level and run the full dispatch pipeline:
    /// history, filter evaluation against managed subscriptions, immediate
    /// delivery or batch accumulation, and metrics.
    ///
    /// Returns the event id.
    pub fn dispatch_event(
        env: Env,
        portfolio_id: Symbol,
        event_type: PortfolioEventType,
        severity: u32,
        details: Map<Symbol, Bytes>,
        metadata: Bytes,
    ) -> Result<u64, Error> {
        let event = Self::emit_event(
            env.clone(),
            portfolio_id.clone(),
            event_type,
            details.clone(),
            metadata,
        )?;

        let sev_key = storage_keys::event_severity(event.event_id);
        env.storage().persistent().set(&sev_key, &severity);

        let now = env.ledger().timestamp();
        let ids = Self::list_managed_subscriptions(env.clone(), portfolio_id.clone());
        for i in 0..ids.len() {
            let sub_id = ids.get(i).unwrap();
            let mut sub = match Self::get_managed_subscription(env.clone(), sub_id) {
                Some(s) => s,
                None => continue,
            };
            if !sub.status.is_receives_events() {
                continue;
            }
            if !sub.filter.matches(&env, &event_type, severity, &details) {
                continue;
            }
            sub.last_event_received_at = Some(now);
            env.storage()
                .persistent()
                .set(&storage_keys::managed_sub(sub_id), &sub);

            let record = DeliveryRecord {
                event_id: event.event_id,
                subscription_id: sub_id,
                subscriber: sub.subscriber.clone(),
                status: DeliveryStatus::Pending,
                attempts: 0,
                next_retry_at: now + backoff_delay(0),
                delivered_at: None,
                acked_at: None,
            };
            Self::enqueue_delivery(env.clone(), record);

            if sub.prefs.mode == DeliveryMode::Batch {
                let batch_key = storage_keys::pending_batch(sub.subscriber.clone());
                let mut batch: Vec<u64> = env
                    .storage()
                    .persistent()
                    .get(&batch_key)
                    .unwrap_or_else(|| Vec::new(&env));
                batch.push_back(event.event_id);
                let reached = batch.len() >= sub.prefs.batch_size;
                env.storage().persistent().set(&batch_key, &batch);
                if reached {
                    Self::flush_batch(env.clone(), sub.subscriber.clone())?;
                }
            }
        }

        let mut metrics = Self::delivery_metrics_raw(&env);
        metrics.total_dispatched += 1;
        env.storage()
            .persistent()
            .set(&storage_keys::delivery_metrics(), &metrics);

        Ok(event.event_id)
    }

    /// Queue (or re-queue) a delivery record.
    fn enqueue_delivery(env: Env, record: DeliveryRecord) {
        let key = storage_keys::deliveries(record.subscriber.clone());
        let mut records: Vec<DeliveryRecord> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        records.push_back(record);
        env.storage().persistent().set(&key, &records);
    }

    /// Attempt delivery of every due `Pending` record for `subscriber`.
    ///
    /// A due record is published to the NOTIFY topic and marked `Delivered`.
    /// Anyone may call this (keeper pattern); each call advances retry state
    /// so failed deliveries back off exponentially.
    pub fn deliver_pending(env: Env, subscriber: Address) -> Result<Vec<u64>, Error> {
        let now = env.ledger().timestamp();
        let key = storage_keys::deliveries(subscriber.clone());
        let records: Vec<DeliveryRecord> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        let mut updated: Vec<DeliveryRecord> = Vec::new(&env);
        let mut delivered: Vec<u64> = Vec::new(&env);
        let mut retries: u32 = 0;

        for i in 0..records.len() {
            let mut rec = records.get(i).unwrap();
            if rec.status == DeliveryStatus::Pending && rec.next_retry_at <= now {
                if rec.attempts > 0 {
                    retries += 1;
                }
                rec.attempts += 1;
                rec.status = DeliveryStatus::Delivered;
                rec.delivered_at = Some(now);
                env.events().publish(
                    (symbol_short!("NOTIFY"), subscriber.clone()),
                    (rec.event_id, rec.subscription_id),
                );
                delivered.push_back(rec.event_id);

                // Per-subscription delivered counter.
                if let Some(mut sub) =
                    Self::get_managed_subscription(env.clone(), rec.subscription_id)
                {
                    sub.total_delivered += 1;
                    env.storage()
                        .persistent()
                        .set(&storage_keys::managed_sub(rec.subscription_id), &sub);
                }
            }
            updated.push_back(rec);
        }

        if !delivered.is_empty() {
            let mut metrics = Self::delivery_metrics_raw(&env);
            metrics.total_delivered += delivered.len();
            metrics.total_retry_attempts = metrics.total_retry_attempts.saturating_add(retries);
            env.storage()
                .persistent()
                .set(&storage_keys::delivery_metrics(), &metrics);
        }

        env.storage().persistent().set(&key, &updated);
        Ok(delivered)
    }

    /// Report a failed delivery attempt; schedules the next retry using
    /// exponential backoff (initial 1s, doubling, max 1 hour).
    pub fn mark_delivery_failed(
        env: Env,
        subscriber: Address,
        event_id: u64,
    ) -> Result<Symbol, Error> {
        let key = storage_keys::deliveries(subscriber.clone());
        let records: Vec<DeliveryRecord> = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotFound)?;

        let mut updated: Vec<DeliveryRecord> = Vec::new(&env);
        let mut found = false;
        for i in 0..records.len() {
            let mut rec = records.get(i).unwrap();
            if !found
                && rec.event_id == event_id
                && rec.subscriber == subscriber
                && rec.status == DeliveryStatus::Pending
            {
                found = true;
                rec.attempts += 1;
                rec.next_retry_at = env.ledger().timestamp() + backoff_delay(rec.attempts - 1);
                let mut metrics = Self::delivery_metrics_raw(&env);
                metrics.total_failed += 1;
                env.storage()
                    .persistent()
                    .set(&storage_keys::delivery_metrics(), &metrics);
                if let Some(mut sub) =
                    Self::get_managed_subscription(env.clone(), rec.subscription_id)
                {
                    sub.total_failed += 1;
                    env.storage()
                        .persistent()
                        .set(&storage_keys::managed_sub(rec.subscription_id), &sub);
                }
            }
            updated.push_back(rec);
        }
        if !found {
            return Err(Error::NotFound);
        }
        env.storage().persistent().set(&key, &updated);
        env.events()
            .publish((symbol_short!("DLV_FAIL"), subscriber), event_id);
        Ok(OK)
    }

    /// Subscriber acknowledges receipt of an event; feeds latency metrics.
    pub fn acknowledge(env: Env, subscriber: Address, event_id: u64) -> Result<Symbol, Error> {
        subscriber.require_auth();
        let key = storage_keys::deliveries(subscriber.clone());
        let records: Vec<DeliveryRecord> = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotFound)?;

        let mut updated: Vec<DeliveryRecord> = Vec::new(&env);
        let mut found = false;
        for i in 0..records.len() {
            let mut rec = records.get(i).unwrap();
            if !found && rec.event_id == event_id && rec.status == DeliveryStatus::Delivered {
                found = true;
                rec.status = DeliveryStatus::Acknowledged;
                rec.acked_at = Some(env.ledger().timestamp());
                let mut metrics = Self::delivery_metrics_raw(&env);
                metrics.total_acknowledged += 1;
                metrics.latency_sum_secs +=
                    env.ledger().timestamp() - rec.delivered_at.unwrap_or(env.ledger().timestamp());
                env.storage()
                    .persistent()
                    .set(&storage_keys::delivery_metrics(), &metrics);
            }
            updated.push_back(rec);
        }
        if !found {
            return Err(Error::InvalidState);
        }
        env.storage().persistent().set(&key, &updated);
        Ok(OK)
    }

    /// Flush a subscriber's accumulated batch: publishes and marks every
    /// queued pending record delivered. Callable once the batch window has
    /// elapsed or early by the owner.
    pub fn flush_batch(env: Env, subscriber: Address) -> Result<Vec<u64>, Error> {
        let batch_key = storage_keys::pending_batch(subscriber.clone());
        let batch: Vec<u64> = env
            .storage()
            .persistent()
            .get(&batch_key)
            .unwrap_or_else(|| Vec::new(&env));
        if batch.is_empty() {
            return Ok(Vec::new(&env));
        }
        let flushed = Self::deliver_pending(env.clone(), subscriber.clone())?;
        env.storage().persistent().remove(&batch_key);
        Ok(flushed)
    }

    /// All delivery records for a subscriber (full audit trail).
    pub fn get_delivery_records(env: Env, subscriber: Address) -> Vec<DeliveryRecord> {
        env.storage()
            .persistent()
            .get(&storage_keys::deliveries(subscriber))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Severity recorded at dispatch time for an event.
    pub fn get_event_severity(env: Env, event_id: u64) -> u32 {
        env.storage()
            .persistent()
            .get(&storage_keys::event_severity(event_id))
            .unwrap_or(0)
    }

    fn delivery_metrics_raw(env: &Env) -> subscriptions::DeliveryMetrics {
        env.storage()
            .persistent()
            .get(&storage_keys::delivery_metrics())
            .unwrap_or_else(subscriptions::DeliveryMetrics::new)
    }

    /// Aggregate delivery metrics snapshot.
    pub fn get_delivery_metrics(env: Env) -> subscriptions::DeliveryMetrics {
        Self::delivery_metrics_raw(&env)
    }
}

// Tests are in contracts/events/tests/integration_tests.rs
