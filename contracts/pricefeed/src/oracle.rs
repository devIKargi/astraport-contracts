//! Oracle provider management and price fetching logic.

use soroban_sdk::{symbol_short, Env, Map, Symbol, Vec};

use crate::records::{
    OracleProvider, PriceDataPoint, PriceFeedDataKey, PriceFeedError, PriceStatus,
};

// ---------------------------------------------------------------------------
// Oracle Manager
// ---------------------------------------------------------------------------

/// Manages oracle provider registration, updates, and price fetching.
pub struct OracleManager;

impl OracleManager {
    /// Register a new oracle provider. Fails if the provider already exists.
    pub fn register_provider(
        env: &Env,
        provider: OracleProvider,
    ) -> Result<Symbol, PriceFeedError> {
        let mut oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));

        if oracles.contains_key(provider.provider_id.clone()) {
            return Err(PriceFeedError::OracleAlreadyExists);
        }

        let id = provider.provider_id.clone();
        oracles.set(id.clone(), provider);
        env.storage()
            .persistent()
            .set(&PriceFeedDataKey::Oracles, &oracles);

        Ok(id)
    }

    /// Update an existing oracle provider's configuration.
    pub fn update_provider(env: &Env, provider: OracleProvider) -> Result<Symbol, PriceFeedError> {
        let mut oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));

        if !oracles.contains_key(provider.provider_id.clone()) {
            return Err(PriceFeedError::OracleNotFound);
        }

        let id = provider.provider_id.clone();
        oracles.set(id.clone(), provider);
        env.storage()
            .persistent()
            .set(&PriceFeedDataKey::Oracles, &oracles);

        Ok(id)
    }

    /// Remove an oracle provider.
    pub fn remove_provider(env: &Env, provider_id: Symbol) -> Result<(), PriceFeedError> {
        let mut oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));

        if !oracles.contains_key(provider_id.clone()) {
            return Err(PriceFeedError::OracleNotFound);
        }

        oracles.remove(provider_id);
        env.storage()
            .persistent()
            .set(&PriceFeedDataKey::Oracles, &oracles);

        Ok(())
    }

    /// Get a single oracle provider by ID.
    pub fn get_provider(env: &Env, provider_id: Symbol) -> Option<OracleProvider> {
        let oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));
        oracles.get(provider_id)
    }

    /// Get all registered oracle providers.
    pub fn get_all_providers(env: &Env) -> Vec<OracleProvider> {
        let oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));

        let mut result = Vec::new(env);
        for (_, provider) in oracles.iter() {
            result.push_back(provider);
        }
        result
    }

    /// Get all active oracle providers.
    pub fn get_active_providers(env: &Env) -> Vec<OracleProvider> {
        let oracles: Map<Symbol, OracleProvider> = env
            .storage()
            .persistent()
            .get(&PriceFeedDataKey::Oracles)
            .unwrap_or_else(|| Map::new(env));

        let mut result = Vec::new(env);
        for (_, provider) in oracles.iter() {
            if provider.is_active {
                result.push_back(provider);
            }
        }
        result
    }

    /// In a real deployment, this would make a cross-contract call to the
    /// oracle endpoint. For this framework, the price data point is submitted
    /// externally via `submit_price` and stored in persistent storage.
    ///
    /// Fetch the most recent price data point for an asset from a given oracle.
    pub fn fetch_price(env: &Env, asset: Symbol, provider_id: Symbol) -> Option<PriceDataPoint> {
        let key = PriceFeedDataKey::LatestDataPoint(asset, provider_id);
        env.storage().persistent().get(&key)
    }

    /// Store a price data point submitted by an oracle (called off-chain or
    /// via cross-contract invocation).
    pub fn submit_price(env: &Env, data_point: PriceDataPoint) -> Symbol {
        let key = PriceFeedDataKey::LatestDataPoint(
            data_point.asset.clone(),
            data_point.provider_id.clone(),
        );
        env.storage().persistent().set(&key, &data_point);
        symbol_short!("ok")
    }

    /// Batch submit multiple price data points (gas optimization).
    pub fn batch_submit_prices(env: &Env, data_points: Vec<PriceDataPoint>) -> u32 {
        let mut count: u32 = 0;
        for dp in data_points.iter() {
            let key = PriceFeedDataKey::LatestDataPoint(dp.asset.clone(), dp.provider_id.clone());
            env.storage().persistent().set(&key, &dp);
            count += 1;
        }
        count
    }

    /// Fetch prices from all active providers for a given asset.
    /// Returns only providers that have data and are not stale.
    pub fn fetch_all_for_asset(env: &Env, asset: Symbol, now: u64) -> Vec<PriceDataPoint> {
        let active = Self::get_active_providers(env);
        let mut results = Vec::new(env);

        for provider in active.iter() {
            if let Some(mut dp) =
                Self::fetch_price(env, asset.clone(), provider.provider_id.clone())
            {
                // Check staleness against provider-specific max_staleness
                if now > 0 && dp.timestamp > 0 {
                    let age = now.saturating_sub(dp.timestamp);
                    if age > provider.max_staleness {
                        dp.status = PriceStatus::Stale;
                    }
                }
                results.push_back(dp);
            }
        }

        results
    }
}
