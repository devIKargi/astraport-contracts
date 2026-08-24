//! Order book management and price-time priority matching.
//!
//! The matching engine uses **price-time priority**: at a given price level,
//! orders are matched in the order they were placed (FIFO). This module
//! operates on in-memory `Vec`s loaded from persistent storage; every
//! mutating function writes the updated book back.

use soroban_sdk::{symbol_short, Address, Env, Symbol, Vec};

use crate::types::*;

/// Maximum order book depth (orders per side) before we stop inserting.
const MAX_DEPTH: u32 = MAX_ORDERS_PER_PAIR;

// ---------------------------------------------------------------------------
// Book loading / saving
// ---------------------------------------------------------------------------

/// Load the order book for `pair_id` from storage.  Returns an empty book
/// if none exists yet.
pub fn load_book(env: &Env, pair_id: &Symbol) -> OrderBook {
    env.storage()
        .persistent()
        .get(&TradeDataKey::OrderBook(pair_id.clone()))
        .unwrap_or_else(|| OrderBook {
            pair_id: pair_id.clone(),
            bids: Vec::new(env),
            asks: Vec::new(env),
        })
}

/// Persist the order book back to storage.
pub fn save_book(env: &Env, book: &OrderBook) {
    env.storage()
        .persistent()
        .set(&TradeDataKey::OrderBook(book.pair_id.clone()), book);
}

/// Get the next order ID for a pair and increment the counter.
pub fn next_order_id(env: &Env, pair_id: &Symbol) -> u64 {
    let key = TradeDataKey::OrderIdCounter(pair_id.clone());
    let current: u64 = env.storage().persistent().get(&key).unwrap_or(1);
    env.storage().persistent().set(&key, &(current + 1));
    current
}

// ---------------------------------------------------------------------------
// Insertion helpers
// ---------------------------------------------------------------------------

/// Insert a new order into the appropriate side of the book, maintaining
/// sorted order.  Bids are sorted descending by price (best bid first);
/// asks are sorted ascending by price (best ask first).
///
/// Returns `Err(TradeError::MaxOrdersReached)` if the book side is at
/// capacity.
fn insert_order_sorted(_env: &Env, book: &mut OrderBook, order: &Order) -> Result<(), TradeError> {
    match order.side {
        OrderSide::Buy => {
            if book.bids.len() >= MAX_DEPTH {
                return Err(TradeError::MaxOrdersReached);
            }
            let mut inserted = false;
            let len = book.bids.len();
            for i in 0..len {
                let existing = book.bids.get(i).unwrap();
                // Insert before the first order with a lower price.
                if order.price > existing.price {
                    book.bids.insert(i, order.clone());
                    inserted = true;
                    break;
                }
            }
            if !inserted {
                book.bids.push_back(order.clone());
            }
        }
        OrderSide::Sell => {
            if book.asks.len() >= MAX_DEPTH {
                return Err(TradeError::MaxOrdersReached);
            }
            let mut inserted = false;
            let len = book.asks.len();
            for i in 0..len {
                let existing = book.asks.get(i).unwrap();
                // Insert before the first order with a higher price.
                if order.price < existing.price {
                    book.asks.insert(i, order.clone());
                    inserted = true;
                    break;
                }
            }
            if !inserted {
                book.asks.push_back(order.clone());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Place a new limit order into the order book.
///
/// Returns the assigned order ID.
pub fn place_order(
    env: &Env,
    pair_id: &Symbol,
    owner: &Address,
    side: OrderSide,
    price: i128,
    amount: i128,
) -> Result<u64, TradeError> {
    if amount <= 0 {
        return Err(TradeError::InvalidOrderAmount);
    }
    if price <= 0 {
        return Err(TradeError::InvalidOrderAmount);
    }

    let mut book = load_book(env, pair_id);
    let order_id = next_order_id(env, pair_id);
    let now = env.ledger().timestamp();

    let order = Order {
        order_id,
        pair_id: pair_id.clone(),
        owner: owner.clone(),
        side,
        price,
        amount,
        remaining: amount,
        status: OrderStatus::Active,
        created_at: now,
    };

    insert_order_sorted(env, &mut book, &order)?;
    save_book(env, &book);

    // Persist the order so it can be looked up individually.
    env.storage()
        .persistent()
        .set(&TradeDataKey::Order(pair_id.clone(), order_id), &order);

    Ok(order_id)
}

/// Cancel an existing active order.  Only the order owner or the contract
/// admin may cancel.
pub fn cancel_order(
    env: &Env,
    pair_id: &Symbol,
    order_id: u64,
    caller: &Address,
    is_admin: bool,
) -> Result<(), TradeError> {
    let mut order: Order = env
        .storage()
        .persistent()
        .get(&TradeDataKey::Order(pair_id.clone(), order_id))
        .ok_or(TradeError::OrderNotFound)?;

    if order.status != OrderStatus::Active && order.status != OrderStatus::PartiallyFilled {
        return Err(TradeError::OrderNotFound);
    }

    if !is_admin && order.owner != *caller {
        return Err(TradeError::NotOrderOwner);
    }

    // Remove from the book.
    let mut book = load_book(env, pair_id);
    remove_order_from_book(&mut book, &order);
    save_book(env, &book);

    // Update the order status.
    order.status = OrderStatus::Cancelled;
    order.remaining = 0;
    env.storage()
        .persistent()
        .set(&TradeDataKey::Order(pair_id.clone(), order_id), &order);

    Ok(())
}

/// Look up a single order by pair and ID.
pub fn get_order(env: &Env, pair_id: &Symbol, order_id: u64) -> Option<Order> {
    env.storage()
        .persistent()
        .get(&TradeDataKey::Order(pair_id.clone(), order_id))
}

/// Return a snapshot of the order book for a pair.
pub fn get_book_snapshot(env: &Env, pair_id: &Symbol) -> OrderBookSnapshot {
    let book = load_book(env, pair_id);
    let best_bid = book.bids.get(0).map(|o| o.price).unwrap_or(0);
    let best_ask = book.asks.get(0).map(|o| o.price).unwrap_or(0);
    let spread = if best_bid > 0 && best_ask > 0 {
        best_ask - best_bid
    } else {
        0
    };
    OrderBookSnapshot {
        pair_id: pair_id.clone(),
        bid_count: book.bids.len(),
        ask_count: book.asks.len(),
        best_bid,
        best_ask,
        spread,
    }
}

// ---------------------------------------------------------------------------
// Matching engine
// ---------------------------------------------------------------------------

/// Try to match an incoming market order against the book.  Returns a `Vec`
/// of `Fill` events.  The incoming order is *not* persisted — it is
/// typically part of an atomic batch whose settlement happens at a higher
/// level.
///
/// For **buy** (taker) orders, we match against the asks from lowest to
/// highest.  For **sell** orders, we match against bids from highest to
/// lowest.
pub fn match_order(
    env: &Env,
    pair_id: &Symbol,
    taker: &Address,
    side: OrderSide,
    amount: i128,
    limit_price: i128,
) -> (Vec<Fill>, i128) {
    let mut book = load_book(env, pair_id);
    let mut fills = Vec::new(env);
    let mut remaining_amount = amount;
    let pair = get_pair_config(env, pair_id);

    match side {
        OrderSide::Buy => {
            // Match against asks (lowest first).
            let mut idx: u32 = 0;
            while remaining_amount > 0 && idx < book.asks.len() {
                let maker_order = book.asks.get(idx).unwrap();
                // Price check: taker willing to pay up to `limit_price`;
                // maker asks `maker_order.price`.  Trade at maker price.
                if maker_order.price > limit_price {
                    break;
                }

                let fill_qty = if remaining_amount <= maker_order.remaining {
                    remaining_amount
                } else {
                    maker_order.remaining
                };

                let fee = calculate_fee(fill_qty, maker_order.price, pair.fee_bps);
                let fill = Fill {
                    order_id: maker_order.order_id,
                    pair_id: pair_id.clone(),
                    taker: taker.clone(),
                    maker: maker_order.owner.clone(),
                    side: OrderSide::Buy,
                    price: maker_order.price,
                    amount: fill_qty,
                    fee,
                };
                fills.push_back(fill);
                remaining_amount -= fill_qty;

                // Update the maker order.
                let updated_remaining = maker_order.remaining - fill_qty;
                let mut updated_order = maker_order.clone();
                updated_order.remaining = updated_remaining;
                if updated_remaining == 0 {
                    updated_order.status = OrderStatus::Filled;
                    book.asks.remove(idx);
                    // Don't increment idx since we removed the element.
                } else {
                    updated_order.status = OrderStatus::PartiallyFilled;
                    book.asks.set(idx, updated_order.clone());
                    idx += 1;
                }
                // Persist the updated maker order.
                env.storage().persistent().set(
                    &TradeDataKey::Order(pair_id.clone(), maker_order.order_id),
                    &updated_order,
                );
            }
        }
        OrderSide::Sell => {
            // Match against bids (highest first).
            let mut idx: u32 = 0;
            while remaining_amount > 0 && idx < book.bids.len() {
                let maker_order = book.bids.get(idx).unwrap();
                // Maker bid must be >= taker's limit price.
                if maker_order.price < limit_price {
                    break;
                }

                let fill_qty = if remaining_amount <= maker_order.remaining {
                    remaining_amount
                } else {
                    maker_order.remaining
                };

                let fee = calculate_fee(fill_qty, maker_order.price, pair.fee_bps);
                let fill = Fill {
                    order_id: maker_order.order_id,
                    pair_id: pair_id.clone(),
                    taker: taker.clone(),
                    maker: maker_order.owner.clone(),
                    side: OrderSide::Sell,
                    price: maker_order.price,
                    amount: fill_qty,
                    fee,
                };
                fills.push_back(fill);
                remaining_amount -= fill_qty;

                let updated_remaining = maker_order.remaining - fill_qty;
                let mut updated_order = maker_order.clone();
                updated_order.remaining = updated_remaining;
                if updated_remaining == 0 {
                    updated_order.status = OrderStatus::Filled;
                    book.bids.remove(idx);
                } else {
                    updated_order.status = OrderStatus::PartiallyFilled;
                    book.bids.set(idx, updated_order.clone());
                    idx += 1;
                }
                env.storage().persistent().set(
                    &TradeDataKey::Order(pair_id.clone(), maker_order.order_id),
                    &updated_order,
                );
            }
        }
    }

    let filled_amount = amount - remaining_amount;
    save_book(env, &book);

    (fills, filled_amount)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Remove an order from the correct side of the book by linear scan.
fn remove_order_from_book(book: &mut OrderBook, order: &Order) {
    match order.side {
        OrderSide::Buy => {
            let len = book.bids.len();
            for i in 0..len {
                if book.bids.get(i).unwrap().order_id == order.order_id {
                    book.bids.remove(i);
                    return;
                }
            }
        }
        OrderSide::Sell => {
            let len = book.asks.len();
            for i in 0..len {
                if book.asks.get(i).unwrap().order_id == order.order_id {
                    book.asks.remove(i);
                    return;
                }
            }
        }
    }
}

/// Look up the pair config for fee calculation.
fn get_pair_config(env: &Env, pair_id: &Symbol) -> TradePair {
    env.storage()
        .persistent()
        .get(&TradeDataKey::Pairs)
        .and_then(|pairs: Vec<TradePair>| {
            let len = pairs.len();
            for i in 0..len {
                let p = pairs.get(i).unwrap();
                if p.pair_id == *pair_id {
                    return Some(p);
                }
            }
            None
        })
        .unwrap_or(TradePair {
            pair_id: pair_id.clone(),
            base_asset: symbol_short!("NONE"),
            quote_asset: symbol_short!("NONE"),
            is_active: true,
            min_order_size: 0,
            max_order_size: i128::MAX,
            fee_bps: 0,
        })
}

/// Calculate the trading fee for a fill: `fill_qty * price * fee_bps /
/// BPS_DENOM`.
fn calculate_fee(fill_qty: i128, price: i128, fee_bps: i128) -> i128 {
    if fee_bps <= 0 {
        return 0;
    }
    // Guard against overflow: use i128 which gives us plenty of room for
    // practical amounts.
    let notional = fill_qty.checked_mul(price).unwrap_or(i128::MAX);
    notional
        .checked_mul(fee_bps)
        .unwrap_or(i128::MAX)
        .checked_div(BPS_DENOM)
        .unwrap_or(i128::MAX)
}
