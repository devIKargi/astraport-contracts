#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Symbol,
};

const ADMIN: Symbol = symbol_short!("ADMIN");
const FEE_IDS: Symbol = symbol_short!("FEE_IDS");
const FEE_STRT: Symbol = symbol_short!("FEE_STRT");
const PORT_FEE: Symbol = symbol_short!("PORT_FEE");
const FEE_HIST: Symbol = symbol_short!("FEE_HIST");
const FEE_WVR: Symbol = symbol_short!("FEE_WVR");
const REV_LEDG: Symbol = symbol_short!("REV_LEDG");
const TOT_COLL: Symbol = symbol_short!("TOT_COLL");
const MAX_HISTORY: u32 = 100;
const MAX_RECIPIENTS: u32 = 20;
const BPS_DENOM: i128 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracterror]
pub enum Error {
    FeeNotFound = 1,
    FeeInactive = 2,
    InvalidFeeConfiguration = 3,
    ArithmeticOverflow = 4,
    FeeWaiverNotFound = 5,
    TooManyRecipients = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub enum FeeType {
    Flat,
    Percentage,
    Tiered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub struct TierEntry {
    pub threshold: i128,
    pub fee_bps: i128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[contracttype]
pub struct FeeStructure {
    pub fee_id: Symbol,
    pub fee_type: FeeType,
    pub amount_bps: i128,
    pub tiered_entries: soroban_sdk::Vec<TierEntry>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[contracttype]
pub struct RevenueRecipient {
    pub address: Address,
    pub share_numerator: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[contracttype]
pub struct FeeRecord {
    pub fee_id: Symbol,
    pub portfolio_id: Symbol,
    pub amount: i128,
    pub calculated_fee: i128,
    pub timestamp: u64,
    pub beneficiary: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[contracttype]
pub struct FeeWaiver {
    pub address: Option<Address>,
    pub portfolio_id: Option<Symbol>,
    pub discount_bps: i128,
    pub waived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[contracttype]
pub struct FeeCalculationResult {
    pub fee_id: Symbol,
    pub gross_amount: i128,
    pub discount_bps: i128,
    pub fee_amount: i128,
    pub waived: bool,
}

fn get_admin(env: &Env) -> Address {
    env.storage().instance().get(&ADMIN).unwrap()
}
fn put_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&ADMIN, admin);
}
fn put_fee_structure(env: &Env, fs: &FeeStructure) {
    let key = (&FEE_STRT, &fs.fee_id);
    env.storage().persistent().set(&key, fs);
}
fn get_fee_structure(env: &Env, fee_id: &Symbol) -> Option<FeeStructure> {
    let key = (&FEE_STRT, fee_id);
    env.storage().persistent().get(&key)
}
fn add_fee_id(env: &Env, fee_id: &Symbol) {
    let mut list: soroban_sdk::Vec<Symbol> = env
        .storage()
        .persistent()
        .get(&FEE_IDS)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    for existing in list.iter() {
        if existing == *fee_id {
            return;
        }
    }
    list.push_back(fee_id.clone());
    env.storage().persistent().set(&FEE_IDS, &list);
}
fn list_all_fee_ids(env: &Env) -> soroban_sdk::Vec<Symbol> {
    env.storage()
        .persistent()
        .get(&FEE_IDS)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env))
}
fn get_portfolio_fee_id(env: &Env, pid: &Symbol) -> Option<Symbol> {
    let key = (&PORT_FEE, pid);
    env.storage().persistent().get(&key)
}
fn set_portfolio_fee_id(env: &Env, pid: &Symbol, fid: &Symbol) {
    let key = (&PORT_FEE, pid);
    env.storage().persistent().set(&key, fid);
}
fn remove_portfolio_fee_id(env: &Env, pid: &Symbol) {
    let key = (&PORT_FEE, pid);
    env.storage().persistent().remove(&key);
}
fn get_fee_history(env: &Env) -> soroban_sdk::Vec<FeeRecord> {
    env.storage()
        .persistent()
        .get(&FEE_HIST)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env))
}
fn append_fee_record(env: &Env, record: &FeeRecord) {
    let mut h = get_fee_history(env);
    if h.len() >= MAX_HISTORY {
        h = h.slice(1..);
    }
    h.push_back(record.clone());
    env.storage().persistent().set(&FEE_HIST, &h);
}
fn get_fee_waivers(env: &Env) -> soroban_sdk::Vec<FeeWaiver> {
    env.storage()
        .persistent()
        .get(&FEE_WVR)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env))
}
fn put_fee_waivers(env: &Env, w: &soroban_sdk::Vec<FeeWaiver>) {
    env.storage().persistent().set(&FEE_WVR, w);
}
fn get_revenue_recipients(env: &Env) -> soroban_sdk::Vec<RevenueRecipient> {
    env.storage()
        .persistent()
        .get(&REV_LEDG)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env))
}
fn put_revenue_recipients(env: &Env, r: &soroban_sdk::Vec<RevenueRecipient>) {
    env.storage().persistent().set(&REV_LEDG, r);
}
fn add_to_total_collected(env: &Env, amount: i128) {
    let cur: i128 = env.storage().instance().get(&TOT_COLL).unwrap_or(0);
    env.storage().instance().set(&TOT_COLL, &(cur + amount));
}

#[contract]
pub struct FeeManagementContract;

#[contractimpl]
impl FeeManagementContract {
    pub fn initialize(env: Env, admin: Address) -> Symbol {
        put_admin(&env, &admin);
        symbol_short!("ok")
    }
    pub fn get_admin(env: Env) -> Address {
        get_admin(&env)
    }
    pub fn transfer_admin(env: Env, new_admin: Address) -> Symbol {
        get_admin(&env).require_auth();
        put_admin(&env, &new_admin);
        symbol_short!("ok")
    }

    pub fn set_fee_structure(
        env: Env,
        fee_id: Symbol,
        fee_type: FeeType,
        amount_bps: i128,
        tiered_entries: soroban_sdk::Vec<TierEntry>,
        active: bool,
    ) -> Symbol {
        get_admin(&env).require_auth();
        match fee_type {
            FeeType::Percentage => {
                if !(0..=BPS_DENOM).contains(&amount_bps) {
                    soroban_sdk::panic_with_error!(&env, Error::InvalidFeeConfiguration);
                }
            }
            FeeType::Flat => {
                if amount_bps < 0 {
                    soroban_sdk::panic_with_error!(&env, Error::InvalidFeeConfiguration);
                }
            }
            FeeType::Tiered => {
                if tiered_entries.is_empty() {
                    soroban_sdk::panic_with_error!(&env, Error::InvalidFeeConfiguration);
                }
            }
        }
        let fs = FeeStructure {
            fee_id: fee_id.clone(),
            fee_type,
            amount_bps,
            tiered_entries,
            active,
        };
        put_fee_structure(&env, &fs);
        add_fee_id(&env, &fee_id);
        symbol_short!("ok")
    }
    pub fn get_fee_structure(env: Env, fee_id: Symbol) -> Option<FeeStructure> {
        get_fee_structure(&env, &fee_id)
    }
    pub fn list_fee_structures(env: Env) -> soroban_sdk::Vec<Symbol> {
        list_all_fee_ids(&env)
    }
    pub fn set_fee_active(env: Env, fee_id: Symbol, active: bool) -> Symbol {
        get_admin(&env).require_auth();
        let mut fs = get_fee_structure(&env, &fee_id)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        fs.active = active;
        put_fee_structure(&env, &fs);
        symbol_short!("ok")
    }

    pub fn set_portfolio_fee(env: Env, portfolio_id: Symbol, fee_id: Symbol) -> Symbol {
        get_admin(&env).require_auth();
        let _ = get_fee_structure(&env, &fee_id)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        set_portfolio_fee_id(&env, &portfolio_id, &fee_id);
        symbol_short!("ok")
    }
    pub fn get_portfolio_fee(env: Env, portfolio_id: Symbol) -> Option<Symbol> {
        get_portfolio_fee_id(&env, &portfolio_id)
    }
    pub fn remove_portfolio_fee(env: Env, portfolio_id: Symbol) -> Symbol {
        get_admin(&env).require_auth();
        remove_portfolio_fee_id(&env, &portfolio_id);
        symbol_short!("ok")
    }

    pub fn calculate_fee(env: Env, fee_id: Symbol, amount: i128) -> FeeCalculationResult {
        let fs = get_fee_structure(&env, &fee_id)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        if !fs.active {
            soroban_sdk::panic_with_error!(&env, Error::FeeInactive);
        }
        let f = Self::clamp_fee(
            Self::compute_raw_fee(
                &env,
                &fs.fee_type,
                &fs.amount_bps,
                &fs.tiered_entries,
                amount,
            ),
            amount,
        );
        FeeCalculationResult {
            fee_id,
            gross_amount: amount,
            discount_bps: 0,
            fee_amount: f,
            waived: false,
        }
    }
    pub fn calculate_portfolio_fee(
        env: Env,
        portfolio_id: Symbol,
        fallback_fee_id: Symbol,
        amount: i128,
    ) -> FeeCalculationResult {
        let fee_id = get_portfolio_fee_id(&env, &portfolio_id).unwrap_or(fallback_fee_id);
        let fs = get_fee_structure(&env, &fee_id)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        if !fs.active {
            soroban_sdk::panic_with_error!(&env, Error::FeeInactive);
        }
        let gf = Self::clamp_fee(
            Self::compute_raw_fee(
                &env,
                &fs.fee_type,
                &fs.amount_bps,
                &fs.tiered_entries,
                amount,
            ),
            amount,
        );
        let (db, w) = Self::resolve_waiver_for_portfolio(&env, &portfolio_id);
        let nf = Self::apply_discount(&env, gf, db, w);
        FeeCalculationResult {
            fee_id,
            gross_amount: amount,
            discount_bps: db,
            fee_amount: nf,
            waived: w,
        }
    }

    pub fn collect_fee(
        env: Env,
        caller: Address,
        fee_id: Symbol,
        portfolio_id: Symbol,
        base_amount: i128,
    ) -> i128 {
        caller.require_auth();
        let fs = get_fee_structure(&env, &fee_id)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        if !fs.active {
            soroban_sdk::panic_with_error!(&env, Error::FeeInactive);
        }
        let gf = Self::clamp_fee(
            Self::compute_raw_fee(
                &env,
                &fs.fee_type,
                &fs.amount_bps,
                &fs.tiered_entries,
                base_amount,
            ),
            base_amount,
        );
        let (db, w) = Self::resolve_waiver_for_collect(&env, &caller, &portfolio_id);
        let nf = Self::apply_discount(&env, gf, db, w);
        append_fee_record(
            &env,
            &FeeRecord {
                fee_id,
                portfolio_id,
                amount: base_amount,
                calculated_fee: nf,
                timestamp: env.ledger().timestamp(),
                beneficiary: caller,
            },
        );
        add_to_total_collected(&env, nf);
        Self::distribute_revenue(&env, nf);
        nf
    }
    pub fn collect_yield_fee(env: Env, caller: Address, portfolio_id: Symbol, y: i128) -> i128 {
        Self::collect_fee(env, caller, symbol_short!("YIELD"), portfolio_id, y)
    }
    pub fn collect_management_fee(
        env: Env,
        caller: Address,
        portfolio_id: Symbol,
        aum: i128,
    ) -> i128 {
        Self::collect_fee(env, caller, symbol_short!("MGMT"), portfolio_id, aum)
    }
    pub fn collect_rebalance_fee(env: Env, caller: Address, portfolio_id: Symbol, t: i128) -> i128 {
        Self::collect_fee(env, caller, symbol_short!("REBAL"), portfolio_id, t)
    }

    pub fn set_fee_waiver(
        env: Env,
        address: Option<Address>,
        portfolio_id: Option<Symbol>,
        discount_bps: i128,
        waived: bool,
    ) -> Symbol {
        get_admin(&env).require_auth();
        if !(0..=BPS_DENOM).contains(&discount_bps) {
            soroban_sdk::panic_with_error!(&env, Error::InvalidFeeConfiguration);
        }
        let w = FeeWaiver {
            address,
            portfolio_id,
            discount_bps,
            waived,
        };
        let mut waivers = get_fee_waivers(&env);
        let mut found = false;
        let mut idx: u32 = 0;
        while idx < waivers.len() {
            let e = waivers.get(idx).unwrap();
            if Self::waiver_matches(&e, &w) {
                waivers.set(idx, w.clone());
                found = true;
                break;
            }
            idx += 1;
        }
        if !found {
            waivers.push_back(w);
        }
        put_fee_waivers(&env, &waivers);
        symbol_short!("ok")
    }
    pub fn remove_fee_waiver(
        env: Env,
        address: Option<Address>,
        portfolio_id: Option<Symbol>,
    ) -> Symbol {
        get_admin(&env).require_auth();
        let waivers = get_fee_waivers(&env);
        let mut nw = soroban_sdk::Vec::new(&env);
        for w in waivers.iter() {
            let t = FeeWaiver {
                address: address.clone(),
                portfolio_id: portfolio_id.clone(),
                discount_bps: 0,
                waived: false,
            };
            if !Self::waiver_matches(&w, &t) {
                nw.push_back(w);
            }
        }
        put_fee_waivers(&env, &nw);
        symbol_short!("ok")
    }
    pub fn list_fee_waivers(env: Env) -> soroban_sdk::Vec<FeeWaiver> {
        get_fee_waivers(&env)
    }

    pub fn set_revenue_recipients(
        env: Env,
        recipients: soroban_sdk::Vec<RevenueRecipient>,
    ) -> Symbol {
        get_admin(&env).require_auth();
        if recipients.len() > MAX_RECIPIENTS {
            soroban_sdk::panic_with_error!(&env, Error::TooManyRecipients);
        }
        put_revenue_recipients(&env, &recipients);
        symbol_short!("ok")
    }
    pub fn list_revenue_recipients(env: Env) -> soroban_sdk::Vec<RevenueRecipient> {
        get_revenue_recipients(&env)
    }
    pub fn distribute_revenue_amount(env: Env, amount: i128) -> soroban_sdk::Vec<(Address, i128)> {
        Self::distribute_revenue(&env, amount)
    }

    pub fn get_total_collected(env: Env) -> i128 {
        env.storage().instance().get(&TOT_COLL).unwrap_or(0)
    }
    pub fn get_fee_history(env: Env, max: u32) -> soroban_sdk::Vec<FeeRecord> {
        let h = get_fee_history(&env);
        if max == 0 || h.len() <= max {
            h
        } else {
            h.slice(h.len() - max..)
        }
    }
    pub fn get_fee_history_count(env: Env) -> u32 {
        get_fee_history(&env).len()
    }

    pub fn estimate_fee(
        env: Env,
        fee_id: Symbol,
        portfolio_id: Option<Symbol>,
        amount: i128,
    ) -> FeeCalculationResult {
        let eid = match &portfolio_id {
            Some(p) => get_portfolio_fee_id(&env, p).unwrap_or(fee_id),
            None => fee_id,
        };
        let fs = get_fee_structure(&env, &eid)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&env, Error::FeeNotFound));
        if !fs.active {
            soroban_sdk::panic_with_error!(&env, Error::FeeInactive);
        }
        let gf = Self::clamp_fee(
            Self::compute_raw_fee(
                &env,
                &fs.fee_type,
                &fs.amount_bps,
                &fs.tiered_entries,
                amount,
            ),
            amount,
        );
        let (db, w) = match &portfolio_id {
            Some(p) => Self::resolve_waiver_for_portfolio(&env, p),
            None => (0, false),
        };
        let nf = Self::apply_discount(&env, gf, db, w);
        FeeCalculationResult {
            fee_id: eid,
            gross_amount: amount,
            discount_bps: db,
            fee_amount: nf,
            waived: w,
        }
    }

    fn compute_raw_fee(
        env: &Env,
        ft: &FeeType,
        ab: &i128,
        te: &soroban_sdk::Vec<TierEntry>,
        amt: i128,
    ) -> i128 {
        match ft {
            FeeType::Flat => *ab,
            FeeType::Percentage => {
                amt.checked_mul(*ab).unwrap_or_else(|| {
                    soroban_sdk::panic_with_error!(env, Error::ArithmeticOverflow)
                }) / BPS_DENOM
            }
            FeeType::Tiered => Self::calculate_tiered_fee(env, te, amt),
        }
    }
    pub(crate) fn clamp_fee(fee: i128, base: i128) -> i128 {
        let f = if fee < 0 { 0 } else { fee };
        if f > base {
            base
        } else {
            f
        }
    }
    fn apply_discount(env: &Env, gf: i128, db: i128, waived: bool) -> i128 {
        if waived {
            0
        } else if db <= 0 {
            gf
        } else {
            let n = gf
                .checked_mul(BPS_DENOM - db)
                .unwrap_or_else(|| soroban_sdk::panic_with_error!(env, Error::ArithmeticOverflow))
                / BPS_DENOM;
            if n < 0 {
                0
            } else {
                n
            }
        }
    }
    fn calculate_tiered_fee(env: &Env, tiers: &soroban_sdk::Vec<TierEntry>, amt: i128) -> i128 {
        let mut abps: i128 = 0;
        let mut found = false;
        let len = tiers.len();
        if len == 0 {
            return 0;
        }
        let mut i = len;
        while i > 0 {
            i -= 1;
            let t = tiers.get(i).unwrap();
            if amt >= t.threshold {
                abps = t.fee_bps;
                found = true;
                break;
            }
        }
        if !found {
            return 0;
        }
        amt.checked_mul(abps)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(env, Error::ArithmeticOverflow))
            / BPS_DENOM
    }
    pub(crate) fn waiver_matches(a: &FeeWaiver, b: &FeeWaiver) -> bool {
        match (&a.address, &b.address) {
            (Some(a1), Some(a2)) => return a1 == a2,
            (None, None) => {}
            _ => return false,
        }
        match (&a.portfolio_id, &b.portfolio_id) {
            (Some(p1), Some(p2)) => p1 == p2,
            (None, None) => true,
            _ => false,
        }
    }
    fn resolve_waiver_for_portfolio(env: &Env, pid: &Symbol) -> (i128, bool) {
        for w in get_fee_waivers(env).iter() {
            if let Some(ref wp) = w.portfolio_id {
                if wp == pid {
                    return (w.discount_bps, w.waived);
                }
            }
        }
        (0, false)
    }
    fn resolve_waiver_for_collect(env: &Env, addr: &Address, pid: &Symbol) -> (i128, bool) {
        for w in get_fee_waivers(env).iter() {
            if let Some(ref wa) = w.address {
                if wa == addr {
                    return (w.discount_bps, w.waived);
                }
            }
            if let Some(ref wp) = w.portfolio_id {
                if wp == pid {
                    return (w.discount_bps, w.waived);
                }
            }
        }
        (0, false)
    }
    fn distribute_revenue(env: &Env, amount: i128) -> soroban_sdk::Vec<(Address, i128)> {
        let recips = get_revenue_recipients(env);
        let mut r = soroban_sdk::Vec::new(env);
        if recips.is_empty() || amount <= 0 {
            return r;
        }
        let mut ts: i128 = 0;
        for rp in recips.iter() {
            ts += rp.share_numerator as i128;
        }
        if ts == 0 {
            return r;
        }
        let mut dist: i128 = 0;
        for rp in recips.iter() {
            let s = (rp.share_numerator as i128)
                .checked_mul(amount)
                .unwrap_or_else(|| soroban_sdk::panic_with_error!(env, Error::ArithmeticOverflow))
                / ts;
            dist += s;
            r.push_back((rp.address, s));
        }
        let rem = amount - dist;
        if rem > 0 && !r.is_empty() {
            let (fa, fs) = r.get(0).unwrap();
            r.set(0, (fa, fs + rem));
        }
        r
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{symbol_short, Env};

    #[test]
    fn test_clamp_fee_negative() {
        assert_eq!(FeeManagementContract::clamp_fee(-100, 1_000), 0);
    }
    #[test]
    fn test_clamp_fee_zero() {
        assert_eq!(FeeManagementContract::clamp_fee(0, 1_000), 0);
    }
    #[test]
    fn test_clamp_fee_within_bounds() {
        assert_eq!(FeeManagementContract::clamp_fee(500, 1_000), 500);
    }
    #[test]
    fn test_clamp_fee_exceeds_amount() {
        assert_eq!(FeeManagementContract::clamp_fee(1_500, 1_000), 1_000);
    }

    #[test]
    fn test_waiver_matches_same_portfolio() {
        let pid = symbol_short!("TEST");
        let w1 = FeeWaiver {
            address: None,
            portfolio_id: Some(pid.clone()),
            discount_bps: 100,
            waived: false,
        };
        let w2 = FeeWaiver {
            address: None,
            portfolio_id: Some(pid),
            discount_bps: 200,
            waived: true,
        };
        assert!(FeeManagementContract::waiver_matches(&w1, &w2));
    }
    #[test]
    fn test_waiver_no_match_different_portfolio() {
        let w1 = FeeWaiver {
            address: None,
            portfolio_id: Some(symbol_short!("P1")),
            discount_bps: 0,
            waived: false,
        };
        let w2 = FeeWaiver {
            address: None,
            portfolio_id: Some(symbol_short!("P2")),
            discount_bps: 0,
            waived: false,
        };
        assert!(!FeeManagementContract::waiver_matches(&w1, &w2));
    }
    #[test]
    fn test_waiver_matches_both_none() {
        let w1 = FeeWaiver {
            address: None,
            portfolio_id: None,
            discount_bps: 0,
            waived: false,
        };
        let w2 = FeeWaiver {
            address: None,
            portfolio_id: None,
            discount_bps: 100,
            waived: true,
        };
        assert!(FeeManagementContract::waiver_matches(&w1, &w2));
    }
    #[test]
    fn test_waiver_no_match_address_vs_none() {
        // Address::from_string requires a valid strkey; test the matching
        // logic by verifying (Some, None) pattern returns false via portfolio
        let w1 = FeeWaiver {
            address: None,
            portfolio_id: Some(symbol_short!("X1")),
            discount_bps: 0,
            waived: false,
        };
        let w2 = FeeWaiver {
            address: None,
            portfolio_id: None,
            discount_bps: 0,
            waived: false,
        };
        // address is (None, None) -> falls through to portfolio check
        // portfolio is (Some, None) -> returns false
        assert!(!FeeManagementContract::waiver_matches(&w1, &w2));
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(Error::FeeNotFound as u32, 1);
        assert_eq!(Error::FeeInactive as u32, 2);
        assert_eq!(Error::InvalidFeeConfiguration as u32, 3);
        assert_eq!(Error::ArithmeticOverflow as u32, 4);
        assert_eq!(Error::FeeWaiverNotFound as u32, 5);
        assert_eq!(Error::TooManyRecipients as u32, 6);
    }
    #[test]
    fn test_fee_type_equality() {
        assert_eq!(FeeType::Flat, FeeType::Flat);
        assert_eq!(FeeType::Percentage, FeeType::Percentage);
        assert_eq!(FeeType::Tiered, FeeType::Tiered);
        assert_ne!(FeeType::Flat, FeeType::Percentage);
    }
    #[test]
    fn test_fee_calculation_result_clone() {
        let result = FeeCalculationResult {
            fee_id: symbol_short!("T"),
            gross_amount: 1_000_000,
            discount_bps: 500,
            fee_amount: 95_000,
            waived: false,
        };
        let cloned = result.clone();
        assert_eq!(result.fee_id, cloned.fee_id);
        assert_eq!(result.fee_amount, cloned.fee_amount);
    }
    #[test]
    fn test_constants() {
        assert_eq!(BPS_DENOM, 10_000);
        assert_eq!(MAX_HISTORY, 100);
        assert_eq!(MAX_RECIPIENTS, 20);
    }
}
