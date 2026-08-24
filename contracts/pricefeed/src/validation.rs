//! Price validation logic: staleness detection, anomaly detection, and
//! fallback mechanisms.

use soroban_sdk::{Env, Symbol, Vec};

use crate::records::{
    AggregatedPrice, PriceDataPoint, PriceFeedDataKey, PriceFeedError, PriceStatus,
    PriceValidationConfig, DEFAULT_CACHE_TTL_SECONDS, DEFAULT_MAX_DEVIATION_BPS, MAX_PRICE_HISTORY,
};

// ---------------------------------------------------------------------------
// Default validation config
// ---------------------------------------------------------------------------

/// Create a default validation configuration.
pub fn default_validation_config() -> PriceValidationConfig {
    PriceValidationConfig {
        max_deviation_bps: DEFAULT_MAX_DEVIATION_BPS,
        default_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        max_history_entries: MAX_PRICE_HISTORY,
        alert_on_anomaly: true,
    }
}

/// Get or create the validation config.
pub fn get_validation_config(env: &Env) -> PriceValidationConfig {
    env.storage()
        .persistent()
        .get(&PriceFeedDataKey::ValidationConfig)
        .unwrap_or_else(|| default_validation_config())
}

/// Save validation config.
pub fn set_validation_config(env: &Env, config: &PriceValidationConfig) {
    env.storage()
        .persistent()
        .set(&PriceFeedDataKey::ValidationConfig, config);
}

// ---------------------------------------------------------------------------
// Staleness checks
// ---------------------------------------------------------------------------

/// Check if a price data point is stale given the current time and the
/// provider's max staleness limit.
pub fn is_stale(env: &Env, data_point: &PriceDataPoint, now: u64) -> bool {
    if now == 0 || data_point.timestamp == 0 {
        return true;
    }
    let age = now.saturating_sub(data_point.timestamp);
    let config = get_validation_config(env);
    // Use the configured default TTL; provider-specific limits are checked in
    // the oracle module when fetching.
    age > config.default_ttl_seconds
}

/// Filter a list of data points, removing any that are stale.
pub fn filter_stale(env: &Env, data_points: &Vec<PriceDataPoint>, now: u64) -> Vec<PriceDataPoint> {
    let mut fresh = Vec::new(env);
    for dp in data_points.iter() {
        if !is_stale(env, &dp, now) {
            fresh.push_back(dp);
        }
    }
    fresh
}

// ---------------------------------------------------------------------------
// Anomaly detection
// ---------------------------------------------------------------------------

/// Calculate the median price from a slice of data points. Returns None if
/// the slice is empty. This operates on the `price` field of each data point.
/// Prices must already be sorted or this will sort them (via a simple
/// insertion-sort on a Vec, acceptable for small oracle counts).
pub fn calculate_median(prices: &Vec<PriceDataPoint>) -> Option<i128> {
    if prices.is_empty() {
        return None;
    }

    let _len = prices.len() as usize;
    let mut sorted: soroban_sdk::Vec<i128> = soroban_sdk::Vec::new(prices.env());
    for dp in prices.iter() {
        sorted.push_back(dp.price);
    }

    // Simple insertion sort (oracle count is typically small: 3-7)
    let mut arr: soroban_sdk::Vec<i128> = soroban_sdk::Vec::new(prices.env());
    for p in sorted.iter() {
        // Insert maintaining order
        let mut inserted = false;
        let len_arr = arr.len();
        if len_arr == 0 {
            arr.push_back(p);
            continue;
        }
        let mut i = 0u32;
        while i < len_arr {
            if p < arr.get(i).unwrap() {
                // Shift: build new vec with insertion
                let mut new_arr: soroban_sdk::Vec<i128> = soroban_sdk::Vec::new(prices.env());
                let mut j = 0u32;
                while j < i {
                    new_arr.push_back(arr.get(j).unwrap());
                    j += 1;
                }
                new_arr.push_back(p);
                while j < len_arr {
                    new_arr.push_back(arr.get(j).unwrap());
                    j += 1;
                }
                arr = new_arr;
                inserted = true;
                break;
            }
            i += 1;
        }
        if !inserted {
            arr.push_back(p);
        }
    }

    let len_arr = arr.len();
    if len_arr == 0 {
        return None;
    }

    if len_arr % 2 == 1 {
        Some(arr.get(len_arr / 2).unwrap())
    } else {
        let mid1 = arr.get(len_arr / 2 - 1).unwrap();
        let mid2 = arr.get(len_arr / 2).unwrap();
        Some((mid1 + mid2) / 2)
    }
}

/// Check if a single price deviates from the median by more than
/// `max_deviation_bps`. Returns true if anomalous.
pub fn is_anomalous(price: i128, median: i128, max_deviation_bps: i128) -> bool {
    if median == 0 {
        return false; // Can't compare against zero median
    }
    let diff = (price - median).abs();
    // deviation in bps = diff * 10000 / median
    let deviation_bps = (diff * 10_000) / median;
    deviation_bps > max_deviation_bps
}

/// Validate a set of data points and return only those that are not anomalous
/// relative to the median. Anomalous points get their status set to Anomalous.
pub fn validate_prices(env: &Env, data_points: &Vec<PriceDataPoint>) -> Vec<PriceDataPoint> {
    let config = get_validation_config(env);

    // First check staleness
    let now = env.ledger().timestamp();
    let fresh = filter_stale(env, data_points, now);

    if fresh.len() == 0 {
        return fresh;
    }

    // Calculate median of fresh prices
    let median = match calculate_median(&fresh) {
        Some(m) => m,
        None => return fresh,
    };

    // Filter out anomalous prices
    let mut valid = Vec::new(env);
    for dp in fresh.iter() {
        if is_anomalous(dp.price, median, config.max_deviation_bps) {
            let mut flagged = dp.clone();
            flagged.status = PriceStatus::Anomalous;
            // Still include it but marked as anomalous for transparency
            valid.push_back(flagged);
        } else {
            valid.push_back(dp);
        }
    }

    valid
}

// ---------------------------------------------------------------------------
// Fallback mechanisms
// ---------------------------------------------------------------------------

/// Set a fallback price for an asset (admin-only, called from the contract).
pub fn set_fallback_price(env: &Env, asset: Symbol, price: i128) {
    let mut fallbacks: soroban_sdk::Map<Symbol, i128> = env
        .storage()
        .persistent()
        .get(&PriceFeedDataKey::FallbackPrices)
        .unwrap_or_else(|| soroban_sdk::Map::new(env));
    fallbacks.set(asset, price);
    env.storage()
        .persistent()
        .set(&PriceFeedDataKey::FallbackPrices, &fallbacks);
}

/// Get the fallback price for an asset, if set.
pub fn get_fallback_price(env: &Env, asset: Symbol) -> Option<i128> {
    let fallbacks: soroban_sdk::Map<Symbol, i128> = env
        .storage()
        .persistent()
        .get(&PriceFeedDataKey::FallbackPrices)
        .unwrap_or_else(|| soroban_sdk::Map::new(env));
    fallbacks.get(asset)
}

/// Attempt to resolve a price for an asset, going through:
/// 1. Fresh aggregated price from active oracles
/// 2. Cached price (if not yet stale beyond a grace period)
/// 3. Fallback price set by admin
///
/// Returns `Err(PriceFeedError::NoPriceData)` only when all sources fail.
pub fn resolve_price(env: &Env, asset: Symbol) -> Result<AggregatedPrice, PriceFeedError> {
    // Try cached price first (it was aggregated at submission time)
    let cached_opt: Option<crate::records::CachedPrice> = env
        .storage()
        .persistent()
        .get(&PriceFeedDataKey::CachedPrices(asset.clone()));

    if let Some(cached) = cached_opt {
        let now = env.ledger().timestamp();
        let age = now.saturating_sub(cached.cached_at);
        // Allow cache to serve up to 2x the TTL as grace period
        let grace_ttl = cached.ttl_seconds * 2;
        if age <= grace_ttl {
            return Ok(cached.aggregated);
        }
    }

    // Try to get fresh data from oracles
    let data_points = crate::oracle::OracleManager::fetch_all_for_asset(
        env,
        asset.clone(),
        env.ledger().timestamp(),
    );
    let valid = validate_prices(env, &data_points);
    let non_anomalous: soroban_sdk::Vec<PriceDataPoint> = {
        let mut v = soroban_sdk::Vec::new(env);
        for dp in valid.iter() {
            if dp.status != PriceStatus::Anomalous {
                v.push_back(dp);
            }
        }
        v
    };

    if non_anomalous.len() > 0 {
        // We have valid oracle data, aggregate it
        let method = get_default_aggregation_method(env);
        let agg = crate::aggregation::AggregateEngine::aggregate(env, &non_anomalous, method);
        return Ok(agg);
    }

    // Last resort: fallback price
    if let Some(fallback_price) = get_fallback_price(env, asset.clone()) {
        return Ok(AggregatedPrice {
            asset,
            price: fallback_price,
            timestamp: env.ledger().timestamp(),
            num_sources: 0,
            method: crate::records::AggregationMethod::Latest,
            status: PriceStatus::Fallback,
        });
    }

    Err(PriceFeedError::NoPriceData)
}

// ---------------------------------------------------------------------------
// History helpers
// ---------------------------------------------------------------------------

/// Record a price into the history buffer for an asset.
pub fn record_price_history(env: &Env, asset: Symbol, price: i128, num_sources: u32) {
    let config = get_validation_config(env);
    let key = PriceFeedDataKey::PriceHistory(asset.clone());
    let mut history: soroban_sdk::Vec<crate::records::PriceHistoryEntry> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));

    let entry = crate::records::PriceHistoryEntry {
        price,
        timestamp: env.ledger().timestamp(),
        num_sources,
    };
    history.push_back(entry);

    // Trim to max history
    while history.len() > config.max_history_entries {
        history.remove(0);
    }

    env.storage().persistent().set(&key, &history);
}

/// Get the price history for an asset.
pub fn get_price_history(
    env: &Env,
    asset: Symbol,
) -> soroban_sdk::Vec<crate::records::PriceHistoryEntry> {
    let key = PriceFeedDataKey::PriceHistory(asset);
    env.storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the default aggregation method.
pub fn get_default_aggregation_method(env: &Env) -> crate::records::AggregationMethod {
    env.storage()
        .persistent()
        .get(&PriceFeedDataKey::DefaultAggregationMethod)
        .unwrap_or(crate::records::AggregationMethod::Median)
}

/// Set the default aggregation method.
pub fn set_default_aggregation_method(env: &Env, method: &crate::records::AggregationMethod) {
    env.storage()
        .persistent()
        .set(&PriceFeedDataKey::DefaultAggregationMethod, method);
}
