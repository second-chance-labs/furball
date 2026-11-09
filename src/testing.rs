//! An in-process fake Kalshi exchange for tests, enabled with the `testing` cargo feature
//! (which also enables `trading`, since the fake mirrors [`crate::trading::TradingClient`]'s
//! order operations).
//!
//! [`FakeExchange`] holds one account's orders on a per-market book and matches limit
//! orders by price and time priority: a bid takes the lowest-priced asks at or below its
//! limit, an ask takes the highest-priced bids at or above its limit, oldest first within
//! a price. Fills execute at the resting (maker) price. The supported time-in-force values
//! are [`TimeInForce::GoodTillCanceled`], [`TimeInForce::ImmediateOrCancel`] and
//! [`TimeInForce::FillOrKill`]. Fees come from [`crate::FeeCalculator`] with the configured
//! fee type and multiplier, one [`crate::FeeAccumulator`] per order.
//!
//! The account's cash, net position, cost basis, realized profit and loss and settlements
//! are tracked. The test drives the exchange: [`FakeExchange::add_market`] registers a
//! market, [`FakeExchange::add_liquidity`] injects counterparty orders so the account's
//! orders have something to match against, [`FakeExchange::set_last_price`] records market
//! data, [`FakeExchange::set_time`] moves the clock (canceling expired orders) and
//! [`FakeExchange::settle`] settles a market at a yes value. Orders may also cross the
//! account's own resting orders (self-trades); both sides fill and both pay fees. The
//! `post_only`, `reduce_only` and self-trade-prevention fields of [`CreateOrderV2`] are
//! ignored: there is only one account, so nothing to trade against itself is prevented
//! and no position to reduce against.
//!
//! The fake is not served over HTTP: reaching it with the real trading client would need
//! a server dependency, which this feature deliberately does not add.
//!
//! ```
//! use furball::testing::FakeExchange;
//! use furball::{Count, OrderBuilder, OrderSide, SelfTradePrevention, TimeInForce};
//! use rust_decimal::Decimal;
//!
//! let exchange = FakeExchange::new(Decimal::new(1_000, 0));
//! exchange.add_market("KXTEST");
//!
//! // The counterparty offers 100 yes at 0.55, then bids them back at 0.65: a two-leg
//! // strategy that captures the spread.
//! exchange
//!     .add_liquidity("KXTEST", OrderSide::Ask, "0.55".parse().unwrap(), Count::from(100))
//!     .unwrap();
//! let buy = OrderBuilder::new(
//!     "KXTEST",
//!     OrderSide::Bid,
//!     Count::from(100),
//!     "0.55".parse().unwrap(),
//!     TimeInForce::GoodTillCanceled,
//!     SelfTradePrevention::TakerAtCross,
//! )
//! .build();
//! let order = exchange.create_order(&buy).unwrap();
//! assert_eq!(order.fill_count, Decimal::new(100, 0));
//!
//! exchange
//!     .add_liquidity("KXTEST", OrderSide::Bid, "0.65".parse().unwrap(), Count::from(100))
//!     .unwrap();
//! let sell = OrderBuilder::new(
//!     "KXTEST",
//!     OrderSide::Ask,
//!     Count::from(100),
//!     "0.65".parse().unwrap(),
//!     TimeInForce::ImmediateOrCancel,
//!     SelfTradePrevention::TakerAtCross,
//! )
//! .build();
//! exchange.create_order(&sell).unwrap();
//!
//! // Bought at 0.55, sold at 0.65: 10.00 of profit against 1.74 + 1.60 of fees.
//! let positions = exchange.positions();
//! let position = positions.first().expect("position");
//! assert_eq!(position.position_fp, Decimal::new(0, 0));
//! assert_eq!(position.realized_pnl_dollars, "10.00".parse().unwrap());
//! assert_eq!(exchange.balance(), "1006.66".parse().unwrap());
//! assert_eq!(exchange.fills(None).len(), 2);
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use thiserror::Error;

use crate::fees::{
    BalancePrecision, FeeAccumulator, FeeBreakdown, FeeCalculator, FeeCoefficients, FeeError,
    FeeSide,
};
use crate::pricing::{Count, Price};
use crate::trading::{CancelledOrder, CreateOrderV2, Order, OrderSide, TimeInForce};
use crate::types::{
    BookSide, FeeType, Fill, MarketPosition, MarketResult, Order as PortfolioOrder, OrderStatus,
    OrderType, Orderbook, OutcomeSide, PriceLevel, Settlement,
};

/// An error from the fake exchange.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ExchangeError {
    /// The market was not added with [`FakeExchange::add_market`].
    #[error("unknown market {ticker}")]
    UnknownMarket {
        /// The market ticker.
        ticker: String,
    },
    /// No order with this id.
    #[error("unknown order {order_id}")]
    UnknownOrder {
        /// The order id.
        order_id: String,
    },
    /// The order is not resting, so it cannot be canceled.
    #[error("order {order_id} is not resting")]
    NotResting {
        /// The order id.
        order_id: String,
    },
    /// The market was already settled.
    #[error("market {ticker} is already settled")]
    AlreadySettled {
        /// The market ticker.
        ticker: String,
    },
    /// The balance cannot cover the order's limit-price cost.
    #[error("insufficient balance for {ticker}: required {required}, available {available}")]
    InsufficientBalance {
        /// The market ticker.
        ticker: String,
        /// The limit-price cost the order would reserve.
        required: Decimal,
        /// The account balance.
        available: Decimal,
    },
    /// The settlement value is outside [0, 1].
    #[error("settlement value {value} is outside [0, 1]")]
    SettlementValueOutOfRange {
        /// The recorded settlement value.
        value: Decimal,
    },
    /// The fee type has no documented formula, so the fill's fee cannot be computed.
    #[error(transparent)]
    Fee(#[from] FeeError),
}

/// A resting order on the book. User orders carry an id into the order map; counterparty
/// liquidity is external and appears only here.
#[derive(Debug, Clone)]
struct BookRef {
    order_id: String,
    price: Decimal,
    sequence: u64,
    remaining: Decimal,
    external: bool,
}

/// One user order's full state.
#[derive(Debug, Clone)]
struct OrderRecord {
    order: PortfolioOrder,
    remaining: Decimal,
    notional: Decimal,
    fees: Decimal,
    accumulator: FeeAccumulator,
    resting: bool,
}

/// The account's open position in one market, average-cost method. `basis` is the cost of
/// an open long or the entry credit of an open short.
#[derive(Debug, Clone)]
struct PositionState {
    net: Decimal,
    basis: Decimal,
    realized: Decimal,
    total_traded: Decimal,
    fees_paid: Decimal,
    last_updated: DateTime<Utc>,
}

impl PositionState {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            net: Decimal::new(0, 0),
            basis: Decimal::new(0, 0),
            realized: Decimal::new(0, 0),
            total_traded: Decimal::new(0, 0),
            fees_paid: Decimal::new(0, 0),
            last_updated: now,
        }
    }

    /// Applies one fill: `delta` contracts on the net yes position (positive for buys),
    /// at `price`, with `fee` charged.
    fn apply(&mut self, delta: Decimal, price: Decimal, fee: Decimal, now: DateTime<Utc>) {
        let zero = Decimal::new(0, 0);
        if delta > zero {
            if self.net < zero {
                let closed = delta.min(-self.net);
                let average = self.basis / -self.net;
                self.realized += closed * (average - price);
                self.basis -= closed * average;
                self.basis += (delta - closed) * price;
            } else {
                self.basis += delta * price;
            }
            self.net += delta;
        } else {
            let d = -delta;
            if self.net > zero {
                let closed = d.min(self.net);
                let average = self.basis / self.net;
                self.realized += closed * (price - average);
                self.basis -= closed * average;
                self.basis += (d - closed) * price;
            } else {
                self.basis += d * price;
            }
            self.net -= d;
        }
        self.total_traded += delta.abs() * price;
        self.fees_paid += fee;
        self.last_updated = now;
    }
}

/// One market's state.
#[derive(Debug, Clone)]
struct MarketState {
    event_ticker: String,
    bids: Vec<BookRef>,
    asks: Vec<BookRef>,
    last_price: Option<Decimal>,
    settled_value: Option<Decimal>,
}

struct Inner {
    now: DateTime<Utc>,
    sequence: u64,
    balance: Decimal,
    fee_type: FeeType,
    fee_multiplier: Decimal,
    precision: BalancePrecision,
    coefficients: FeeCoefficients,
    markets: HashMap<String, MarketState>,
    positions: HashMap<String, PositionState>,
    orders: HashMap<String, OrderRecord>,
    fills: Vec<Fill>,
    settlements: Vec<Settlement>,
    order_counter: u64,
    fill_counter: u64,
}

/// An in-process fake Kalshi exchange: one account, per-market books, price-and-time
/// priority matching, fees from the fee calculator, and a test-controlled clock, market
/// data and settlement. See the module docs for the model.
pub struct FakeExchange {
    inner: Mutex<Inner>,
}

impl fmt::Debug for FakeExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeExchange").finish_non_exhaustive()
    }
}

impl FakeExchange {
    /// Starts a fake exchange with `starting_balance` in the account, the default
    /// coefficients, a quadratic fee at multiplier 1, cent precision, and the clock at
    /// 2026-01-01T00:00:00Z.
    pub fn new(starting_balance: Decimal) -> Self {
        Self {
            inner: Mutex::new(Inner {
                now: "2026-01-01T00:00:00Z".parse().expect("fixed clock"),
                sequence: 0,
                balance: starting_balance,
                fee_type: FeeType::Quadratic,
                fee_multiplier: Decimal::new(1, 0),
                precision: BalancePrecision::Cent,
                coefficients: FeeCoefficients::default(),
                markets: HashMap::new(),
                positions: HashMap::new(),
                orders: HashMap::new(),
                fills: Vec::new(),
                settlements: Vec::new(),
                order_counter: 0,
                fill_counter: 0,
            }),
        }
    }

    /// The fee type and multiplier every fill is charged at.
    pub fn fee_schedule(self, fee_type: FeeType, multiplier: Decimal) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.fee_type = fee_type;
            inner.fee_multiplier = multiplier;
        }
        self
    }

    /// The fee coefficients.
    pub fn coefficients(self, coefficients: FeeCoefficients) -> Self {
        self.inner.lock().unwrap().coefficients = coefficients;
        self
    }

    /// The balance precision fees align to.
    pub fn precision(self, precision: BalancePrecision) -> Self {
        self.inner.lock().unwrap().precision = precision;
        self
    }

    /// Starts the clock at `at` instead of the fixed default.
    pub fn clock(self, at: DateTime<Utc>) -> Self {
        self.inner.lock().unwrap().now = at;
        self
    }

    /// Registers a market so orders can be placed on it.
    pub fn add_market(&self, ticker: impl Into<String>) {
        let ticker = ticker.into();
        let mut inner = self.inner.lock().unwrap();
        inner.markets.insert(
            ticker.clone(),
            MarketState {
                event_ticker: format!("{ticker}-E"),
                bids: Vec::new(),
                asks: Vec::new(),
                last_price: None,
                settled_value: None,
            },
        );
    }

    /// Records the last traded yes price for a market.
    pub fn set_last_price(&self, ticker: &str, yes_price: Decimal) -> Result<(), ExchangeError> {
        let mut inner = self.inner.lock().unwrap();
        let market = inner
            .markets
            .get_mut(ticker)
            .ok_or_else(|| ExchangeError::UnknownMarket {
                ticker: ticker.to_owned(),
            })?;
        market.last_price = Some(yes_price);
        Ok(())
    }

    /// The last recorded yes price for a market, or `None` for an unknown market or one
    /// with no recorded price.
    pub fn last_price(&self, ticker: &str) -> Option<Decimal> {
        self.inner
            .lock()
            .unwrap()
            .markets
            .get(ticker)
            .and_then(|market| market.last_price)
    }

    /// Injects counterparty liquidity: an order owned by the market, not the account. It
    /// matches the account's orders by price and time priority like any resting order, but
    /// its fills appear in no fill list, change no position and pay no fee.
    pub fn add_liquidity(
        &self,
        ticker: &str,
        side: OrderSide,
        price: Price,
        count: Count,
    ) -> Result<(), ExchangeError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.markets.contains_key(ticker) {
            return Err(ExchangeError::UnknownMarket {
                ticker: ticker.to_owned(),
            });
        }
        if inner.markets[ticker].settled_value.is_some() {
            return Err(ExchangeError::AlreadySettled {
                ticker: ticker.to_owned(),
            });
        }
        inner.sequence += 1;
        let reference = BookRef {
            order_id: format!("fake-liquidity-{}", inner.sequence),
            price: price.as_dollars(),
            sequence: inner.sequence,
            remaining: count.as_contracts(),
            external: true,
        };
        let market = inner.markets.get_mut(ticker).expect("market exists");
        match side {
            OrderSide::Bid => market.bids.push(reference),
            OrderSide::Ask => market.asks.push(reference),
        }
        Ok(())
    }

    /// Moves the clock to `at`. Resting orders whose `expiration_time` has passed are
    /// canceled; buying orders release their reserved cash.
    pub fn set_time(&self, at: DateTime<Utc>) {
        let mut inner = self.inner.lock().unwrap();
        inner.now = at;
        let expired: Vec<String> = inner
            .orders
            .values()
            .filter(|record| record.resting)
            .filter(|record| {
                record
                    .order
                    .expiration_time
                    .map(|expiration| at >= expiration)
                    .unwrap_or(false)
            })
            .map(|record| record.order.order_id.clone())
            .collect();
        for order_id in expired {
            inner.cancel_resting(&order_id);
        }
    }

    /// The clock's current time.
    pub fn time(&self) -> DateTime<Utc> {
        self.inner.lock().unwrap().now
    }

    /// Places an order, matching it against the book before resting what is left. Mirrors
    /// [`crate::trading::TradingClient::create_order`]: the response carries the immediate
    /// fill count, the remaining count and the average fill price and fee.
    pub fn create_order(&self, order: &CreateOrderV2) -> Result<Order, ExchangeError> {
        let mut inner = self.inner.lock().unwrap();
        let price = order.price.as_dollars();
        let count = order.count.as_contracts();
        let zero = Decimal::new(0, 0);
        let is_bid = order.side == OrderSide::Bid;
        {
            let market =
                inner
                    .markets
                    .get(&order.ticker)
                    .ok_or_else(|| ExchangeError::UnknownMarket {
                        ticker: order.ticker.clone(),
                    })?;
            if market.settled_value.is_some() {
                return Err(ExchangeError::AlreadySettled {
                    ticker: order.ticker.clone(),
                });
            }
        }

        inner.order_counter += 1;
        let order_id = format!("fake-order-{}", inner.order_counter);
        let (outcome_side, book_side) = match order.side {
            OrderSide::Bid => (OutcomeSide::Yes, BookSide::Bid),
            OrderSide::Ask => (OutcomeSide::No, BookSide::Ask),
        };
        let mut record = OrderRecord {
            order: PortfolioOrder {
                order_id: order_id.clone(),
                user_id: "fake-user".to_owned(),
                client_order_id: order.client_order_id.clone().unwrap_or_default(),
                ticker: order.ticker.clone(),
                outcome_side,
                book_side,
                order_type: OrderType::Limit,
                status: OrderStatus::Resting,
                yes_price_dollars: price,
                no_price_dollars: Decimal::new(1, 0) - price,
                fill_count_fp: zero,
                remaining_count_fp: count,
                initial_count_fp: count,
                taker_fill_cost_dollars: zero,
                maker_fill_cost_dollars: zero,
                taker_fees_dollars: zero,
                maker_fees_dollars: zero,
                expiration_time: order
                    .expiration_time
                    .and_then(DateTime::from_timestamp_millis),
                created_time: Some(inner.now),
                last_update_time: Some(inner.now),
                order_group_id: None,
                subaccount_number: None,
                exchange_index: None,
            },
            remaining: count,
            notional: zero,
            fees: zero,
            accumulator: FeeAccumulator::new(inner.precision),
            resting: false,
        };

        if count > zero {
            if is_bid {
                let required = count * price;
                if inner.balance < required {
                    return Err(ExchangeError::InsufficientBalance {
                        ticker: order.ticker.clone(),
                        required,
                        available: inner.balance,
                    });
                }
                inner.balance -= required;
            }
            let expired = record
                .order
                .expiration_time
                .map(|expiration| inner.now >= expiration)
                .unwrap_or(false);
            if expired {
                record.order.status = OrderStatus::Canceled;
                if is_bid {
                    inner.balance += record.remaining * price;
                }
            } else {
                let fillable = if order.time_in_force == TimeInForce::FillOrKill {
                    inner.available_within(&order.ticker, order.side, price) >= count
                } else {
                    true
                };
                if fillable {
                    record.remaining = inner
                        .match_order(
                            &order.ticker,
                            order.side,
                            price,
                            order_id.clone(),
                            &mut record,
                        )
                        .map_err(ExchangeError::Fee)?;
                }
                if record.remaining == zero {
                    record.order.status = OrderStatus::Executed;
                } else if fillable && order.time_in_force == TimeInForce::GoodTillCanceled {
                    inner.sequence += 1;
                    let reference = BookRef {
                        order_id: order_id.clone(),
                        price,
                        sequence: inner.sequence,
                        remaining: record.remaining,
                        external: false,
                    };
                    let market = inner
                        .markets
                        .get_mut(&order.ticker)
                        .expect("market checked above");
                    if is_bid {
                        market.bids.push(reference);
                    } else {
                        market.asks.push(reference);
                    }
                    record.resting = true;
                } else {
                    record.order.status = OrderStatus::Canceled;
                    if is_bid {
                        inner.balance += record.remaining * price;
                    }
                }
            }
        } else {
            record.order.status = OrderStatus::Executed;
        }

        let filled = count - record.remaining;
        let response = Order {
            order_id,
            client_order_id: order.client_order_id.clone(),
            fill_count: filled,
            remaining_count: record.remaining,
            average_fill_price: if filled > zero {
                Some(record.notional / filled)
            } else {
                None
            },
            average_fee_paid: if filled > zero {
                Some(record.fees / filled)
            } else {
                None
            },
            ts_ms: inner.now.timestamp_millis(),
        };
        record.order.remaining_count_fp = record.remaining;
        record.order.last_update_time = Some(inner.now);
        inner.orders.insert(record.order.order_id.clone(), record);
        Ok(response)
    }

    /// Cancels a resting order, releasing the reserved cash of a buying order. Mirrors
    /// [`crate::trading::TradingClient::cancel_order`].
    pub fn cancel_order(&self, order_id: &str) -> Result<CancelledOrder, ExchangeError> {
        let mut inner = self.inner.lock().unwrap();
        let client_order_id = inner
            .orders
            .get(order_id)
            .ok_or_else(|| ExchangeError::UnknownOrder {
                order_id: order_id.to_owned(),
            })?
            .order
            .client_order_id
            .clone();
        if !inner.orders.get(order_id).expect("checked above").resting {
            return Err(ExchangeError::NotResting {
                order_id: order_id.to_owned(),
            });
        }
        let reduced_by = inner.cancel_resting(order_id);
        Ok(CancelledOrder {
            order_id: order_id.to_owned(),
            client_order_id: Some(client_order_id),
            reduced_by,
        })
    }

    /// Cancels every resting order in the account. Returns how many were canceled.
    pub fn cancel_all(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let resting: Vec<String> = inner
            .orders
            .values()
            .filter(|record| record.resting)
            .map(|record| record.order.order_id.clone())
            .collect();
        for order_id in &resting {
            inner.cancel_resting(order_id);
        }
        resting.len()
    }

    /// One order by id, whatever its status.
    pub fn order(&self, order_id: &str) -> Result<PortfolioOrder, ExchangeError> {
        let inner = self.inner.lock().unwrap();
        inner
            .orders
            .get(order_id)
            .map(|record| record.order.clone())
            .ok_or_else(|| ExchangeError::UnknownOrder {
                order_id: order_id.to_owned(),
            })
    }

    /// The account's orders, newest first, optionally restricted to one market.
    pub fn orders(&self, ticker: Option<&str>) -> Vec<PortfolioOrder> {
        let inner = self.inner.lock().unwrap();
        let mut orders: Vec<PortfolioOrder> = inner
            .orders
            .values()
            .filter(|record| ticker.map(|t| record.order.ticker == t).unwrap_or(true))
            .map(|record| record.order.clone())
            .collect();
        orders.sort_by(|a, b| {
            b.created_time
                .or(b.last_update_time)
                .unwrap_or_else(default_time)
                .cmp(
                    &a.created_time
                        .or(a.last_update_time)
                        .unwrap_or_else(default_time),
                )
        });
        orders
    }

    /// The account's fills, oldest first, optionally restricted to one market.
    pub fn fills(&self, ticker: Option<&str>) -> Vec<Fill> {
        let inner = self.inner.lock().unwrap();
        inner
            .fills
            .iter()
            .filter(|fill| ticker.map(|t| fill.ticker == *t).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// The market's book: yes bids on `yes_dollars`, no bids on `no_dollars` (each no bid
    /// priced as `1 -` its yes ask), best level first.
    pub fn order_book(&self, ticker: &str) -> Result<Orderbook, ExchangeError> {
        let inner = self.inner.lock().unwrap();
        let market = inner
            .markets
            .get(ticker)
            .ok_or_else(|| ExchangeError::UnknownMarket {
                ticker: ticker.to_owned(),
            })?;
        let mut yes_dollars: Vec<PriceLevel> = market
            .bids
            .iter()
            .map(|reference| PriceLevel {
                price_dollars: reference.price,
                count_fp: reference.remaining,
            })
            .collect();
        yes_dollars.sort_by_key(|level| std::cmp::Reverse(level.price_dollars));
        let mut no_dollars: Vec<PriceLevel> = market
            .asks
            .iter()
            .map(|reference| PriceLevel {
                price_dollars: Decimal::new(1, 0) - reference.price,
                count_fp: reference.remaining,
            })
            .collect();
        no_dollars.sort_by_key(|level| std::cmp::Reverse(level.price_dollars));
        Ok(Orderbook {
            yes_dollars,
            no_dollars,
        })
    }

    /// The account balance, in dollars.
    pub fn balance(&self) -> Decimal {
        self.inner.lock().unwrap().balance
    }

    /// The account's positions, one per market that has traded, sorted by ticker.
    pub fn positions(&self) -> Vec<MarketPosition> {
        let inner = self.inner.lock().unwrap();
        let zero = Decimal::new(0, 0);
        let mut positions = inner
            .positions
            .iter()
            .filter(|(_, state)| {
                state.net != zero || state.fees_paid != zero || state.total_traded != zero
            })
            .map(|(ticker, state)| MarketPosition {
                ticker: ticker.clone(),
                exchange_index: 0,
                total_traded_dollars: state.total_traded,
                position_fp: state.net,
                market_exposure_dollars: state.basis,
                realized_pnl_dollars: state.realized,
                fees_paid_dollars: state.fees_paid,
                last_updated_ts: state.last_updated,
                netting_enabled: true,
            })
            .collect::<Vec<_>>();
        positions.sort_by(|a, b| a.ticker.cmp(&b.ticker));
        positions
    }

    /// Settles a market at `yes_value`, the payout of one yes contract in dollars: `1.00`
    /// for a yes result, `0.00` for a no result, or the recorded value for a scalar
    /// market. The net position is paid out (a no position is a negative net yes position,
    /// whose face value its fills already credited), every resting order in the market is
    /// canceled, and a [`Settlement`] is recorded. The fake charges no settlement fee.
    pub fn settle(&self, ticker: &str, yes_value: Decimal) -> Result<Settlement, ExchangeError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.markets.contains_key(ticker) {
            return Err(ExchangeError::UnknownMarket {
                ticker: ticker.to_owned(),
            });
        }
        if inner.markets[ticker].settled_value.is_some() {
            return Err(ExchangeError::AlreadySettled {
                ticker: ticker.to_owned(),
            });
        }
        if yes_value < Decimal::new(0, 0) || yes_value > Decimal::new(1, 0) {
            return Err(ExchangeError::SettlementValueOutOfRange { value: yes_value });
        }
        let resting: Vec<String> = {
            let market = &inner.markets[ticker];
            market
                .bids
                .iter()
                .chain(market.asks.iter())
                .filter(|reference| !reference.external)
                .map(|reference| reference.order_id.clone())
                .collect()
        };
        for order_id in &resting {
            inner.cancel_resting(order_id);
        }
        let event_ticker = {
            let market = inner.markets.get_mut(ticker).expect("market exists");
            market.settled_value = Some(yes_value);
            market.bids.clear();
            market.asks.clear();
            market.event_ticker.clone()
        };

        let now = inner.now;
        let (net, basis) = {
            let state = inner
                .positions
                .entry(ticker.to_owned())
                .or_insert_with(|| PositionState::new(now));
            (state.net, state.basis)
        };
        let payout = net * yes_value;
        inner.balance += payout;
        let market_result = if yes_value == Decimal::new(1, 0) {
            MarketResult::Yes
        } else if yes_value == Decimal::new(0, 0) {
            MarketResult::No
        } else {
            MarketResult::Scalar
        };
        let settlement = Settlement {
            ticker: ticker.to_owned(),
            exchange_index: 0,
            event_ticker,
            market_result,
            yes_count_fp: net.max(Decimal::new(0, 0)),
            yes_total_cost_dollars: if net > Decimal::new(0, 0) {
                basis
            } else {
                Decimal::new(0, 0)
            },
            no_count_fp: (-net).max(Decimal::new(0, 0)),
            no_total_cost_dollars: if net < Decimal::new(0, 0) {
                basis
            } else {
                Decimal::new(0, 0)
            },
            revenue: (payout * Decimal::new(100, 0))
                .round()
                .to_i64()
                .expect("payout cents fit an i64"),
            settled_time: inner.now,
            fee_cost: Decimal::new(0, 0),
            value: Some(
                (yes_value * Decimal::new(100, 0))
                    .round()
                    .to_i64()
                    .expect("settlement cents fit an i64"),
            ),
        };
        {
            let state = inner.positions.get_mut(ticker).expect("position exists");
            state.net = Decimal::new(0, 0);
            state.basis = Decimal::new(0, 0);
            state.last_updated = now;
        }
        inner.settlements.push(settlement.clone());
        Ok(settlement)
    }

    /// The settlements recorded so far, oldest first.
    pub fn settlements(&self) -> Vec<Settlement> {
        self.inner.lock().unwrap().settlements.clone()
    }
}

fn default_time() -> DateTime<Utc> {
    "1970-01-01T00:00:00Z".parse().expect("fixed time")
}

impl Inner {
    /// Total resting contracts on the opposite book that `side` at `price` can take.
    fn available_within(&self, ticker: &str, side: OrderSide, price: Decimal) -> Decimal {
        let Some(market) = self.markets.get(ticker) else {
            return Decimal::new(0, 0);
        };
        let book = match side {
            OrderSide::Bid => &market.asks,
            OrderSide::Ask => &market.bids,
        };
        book.iter()
            .filter(|reference| match side {
                OrderSide::Bid => reference.price <= price,
                OrderSide::Ask => reference.price >= price,
            })
            .map(|reference| reference.remaining)
            .sum()
    }

    /// Matches a taker `side` at limit `price` against the opposite book, best price then
    /// oldest first, filling at the resting price. Returns the unfilled remainder.
    fn match_order(
        &mut self,
        ticker: &str,
        side: OrderSide,
        price: Decimal,
        taker_id: String,
        taker: &mut OrderRecord,
    ) -> Result<Decimal, FeeError> {
        let mut remaining = taker.remaining;
        let zero = Decimal::new(0, 0);
        let is_taker_bid = side == OrderSide::Bid;
        while remaining > zero {
            let best = {
                let market = self.markets.get_mut(ticker).expect("market exists");
                let book = if is_taker_bid {
                    &mut market.asks
                } else {
                    &mut market.bids
                };
                let best_index = (0..book.len())
                    .filter(|&index| {
                        if is_taker_bid {
                            book[index].price <= price
                        } else {
                            book[index].price >= price
                        }
                    })
                    .min_by_key(|&index| {
                        (
                            if is_taker_bid {
                                book[index].price
                            } else {
                                -book[index].price
                            },
                            book[index].sequence,
                        )
                    });
                best_index.map(|index| book.remove(index))
            };
            let Some(mut reference) = best else {
                break;
            };
            let fill = remaining.min(reference.remaining);
            self.execute_fill(
                ticker,
                reference.clone(),
                is_taker_bid,
                taker_id.clone(),
                taker,
                fill,
            )?;
            remaining -= fill;
            taker.remaining = remaining;
            reference.remaining -= fill;
            if reference.remaining > Decimal::new(0, 0) {
                let market = self.markets.get_mut(ticker).expect("market exists");
                if is_taker_bid {
                    market.asks.push(reference);
                } else {
                    market.bids.push(reference);
                }
            }
        }
        Ok(remaining)
    }

    /// Executes one fill between the taker record and the maker book reference, at the
    /// resting price.
    fn execute_fill(
        &mut self,
        ticker: &str,
        maker: BookRef,
        is_taker_bid: bool,
        taker_id: String,
        taker: &mut OrderRecord,
        fill: Decimal,
    ) -> Result<(), FeeError> {
        let price = maker.price;
        self.sequence += 1;
        let trade_id = format!("fake-trade-{}", self.sequence);
        let calculator = FeeCalculator::new(self.coefficients.clone());
        let price_value = Price::new(price).expect("resting prices are on (0, 1)");
        let count_value = Count::new(fill).expect("fills are positive");
        let taker_revenue = if is_taker_bid {
            -(fill * price)
        } else {
            fill * price
        };

        let taker_fee = calculator.fill_fee(
            self.fee_type.clone(),
            self.fee_multiplier,
            FeeSide::Taker,
            price_value,
            count_value,
            taker_revenue,
            self.precision,
            &mut taker.accumulator,
        )?;
        let maker_fee: Option<FeeBreakdown> = if maker.external {
            None
        } else {
            let record = self
                .orders
                .get_mut(&maker.order_id)
                .expect("maker order exists");
            Some(calculator.fill_fee(
                self.fee_type.clone(),
                self.fee_multiplier,
                FeeSide::Maker,
                Price::new(price).expect("resting prices are on (0, 1)"),
                Count::new(fill).expect("fills are positive"),
                -taker_revenue,
                self.precision,
                &mut record.accumulator,
            )?)
        };

        let no_price = Decimal::new(1, 0) - price;
        let (taker_outcome, taker_book_side) = match is_taker_bid {
            true => (OutcomeSide::Yes, BookSide::Bid),
            false => (OutcomeSide::No, BookSide::Ask),
        };
        self.record_fill(
            ticker,
            trade_id.clone(),
            taker_id.clone(),
            taker_outcome,
            taker_book_side,
            fill,
            price,
            no_price,
            true,
            taker_fee.net_fee,
        );
        taker.notional += fill * price;
        taker.fees += taker_fee.net_fee;
        taker.order.fill_count_fp += fill;
        taker.order.remaining_count_fp = taker.remaining - fill;
        taker.order.last_update_time = Some(self.now);
        taker.order.taker_fees_dollars += taker_fee.net_fee;
        let taker_cost = if is_taker_bid {
            fill * price
        } else {
            -(fill * price)
        };
        taker.order.taker_fill_cost_dollars += taker_cost;

        if is_taker_bid {
            self.balance += fill * (taker.order.yes_price_dollars - price);
        } else {
            self.balance += fill * price;
        }
        self.balance -= taker_fee.net_fee;
        let state = self
            .positions
            .entry(ticker.to_owned())
            .or_insert_with(|| PositionState::new(self.now));
        state.apply(
            if is_taker_bid { fill } else { -fill },
            price,
            taker_fee.net_fee,
            self.now,
        );

        if let Some(maker_fee) = maker_fee {
            let maker_id = maker.order_id.clone();
            let (maker_outcome, maker_book_side, maker_delta) = if is_taker_bid {
                (OutcomeSide::No, BookSide::Ask, -fill)
            } else {
                (OutcomeSide::Yes, BookSide::Bid, fill)
            };
            self.record_fill(
                ticker,
                trade_id,
                maker_id.clone(),
                maker_outcome,
                maker_book_side,
                fill,
                price,
                no_price,
                false,
                maker_fee.net_fee,
            );
            let record = self.orders.get_mut(&maker_id).expect("maker order exists");
            record.remaining -= fill;
            record.notional += fill * price;
            record.fees += maker_fee.net_fee;
            record.order.fill_count_fp += fill;
            record.order.remaining_count_fp = record.remaining;
            record.order.last_update_time = Some(self.now);
            if record.remaining == Decimal::new(0, 0) {
                record.order.status = OrderStatus::Executed;
                record.resting = false;
            }
            record.order.maker_fees_dollars += maker_fee.net_fee;
            let maker_cost = if is_taker_bid {
                -(fill * price)
            } else {
                fill * price
            };
            record.order.maker_fill_cost_dollars += maker_cost;
            if is_taker_bid {
                self.balance += fill * price;
            } else {
                self.balance += fill * (record.order.yes_price_dollars - price);
            }
            self.balance -= maker_fee.net_fee;
            let state = self
                .positions
                .entry(ticker.to_owned())
                .or_insert_with(|| PositionState::new(self.now));
            state.apply(maker_delta, price, maker_fee.net_fee, self.now);
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "every fill field is an independent input"
    )]
    fn record_fill(
        &mut self,
        ticker: &str,
        trade_id: String,
        order_id: String,
        outcome_side: OutcomeSide,
        book_side: BookSide,
        count: Decimal,
        yes_price: Decimal,
        no_price: Decimal,
        is_taker: bool,
        fee_cost: Decimal,
    ) {
        self.fill_counter += 1;
        self.fills.push(Fill {
            fill_id: format!("fake-fill-{}", self.fill_counter),
            exchange_index: 0,
            trade_id,
            order_id,
            ticker: ticker.to_owned(),
            market_ticker: ticker.to_owned(),
            outcome_side,
            book_side,
            count_fp: count,
            yes_price_dollars: yes_price,
            no_price_dollars: no_price,
            is_taker,
            fee_cost,
            created_time: Some(self.now),
            subaccount_number: None,
        });
    }

    /// Cancels a resting order, releasing the reserved cash of a buying order. Returns
    /// the contracts removed.
    fn cancel_resting(&mut self, order_id: &str) -> Decimal {
        let record = self.orders.get_mut(order_id).expect("resting order exists");
        let (is_bid, price) = (
            record.order.outcome_side == OutcomeSide::Yes,
            record.order.yes_price_dollars,
        );
        let reduced_by = record.remaining;
        if is_bid {
            self.balance += reduced_by * price;
        }
        if let Some(market) = self.markets.get_mut(&record.order.ticker) {
            market
                .bids
                .retain(|reference| reference.order_id != order_id);
            market
                .asks
                .retain(|reference| reference.order_id != order_id);
        }
        record.resting = false;
        record.order.status = OrderStatus::Canceled;
        record.order.last_update_time = Some(self.now);
        reduced_by
    }
}
