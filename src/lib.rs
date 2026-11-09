#![doc = include_str!("../README.md")]
//! Transport client for Kalshi's Trade API v2 (read-only endpoint surface for now).
//!
//! - [`Client`] signs requests ([`Credentials`]), throttles them with independent Read, Write and
//!   balance token buckets sized by [`Tier`], and retries HTTP 429 with exponential backoff.
//! - The transport sends GET, POST, PUT and DELETE with optional JSON bodies. A failure of a
//!   non-idempotent write after dispatch surfaces as [`KalshiError::Ambiguous`] instead of being
//!   retried, so callers can reconcile before retrying.
//! - [`Client::paginate`] follows cursors as a lazy stream.
//! - [`Client::market`], [`Client::candlesticks`] and [`Client::trades`] hide the split between live
//!   and historical endpoints using the cached [`Cutoff`].
//! - The trading write surface (`TradingClient`) is behind the `trading` cargo feature, off by
//!   default: a default build contains no write methods.
//! - Authenticated portfolio reads: [`Client::balance`] (its own per-second bucket),
//!   [`Client::positions`], [`Client::settlements`] and [`Client::subaccount_balances`].
//! - Authenticated order and fill reads, live and archived: [`Client::get_order`],
//!   [`Client::orders`], [`Client::fills`], [`Client::position_history`] and the queue-position
//!   endpoints, split at the cached [`Cutoff`] and merged like trades.
//!
//! ```text
//! caller -> routing (cutoff cache) -> pagination -> send_text (bucket -> sign -> HTTP -> 429 retry)
//! ```

mod auth;
mod client;
mod endpoints;
mod error;
mod fee_schedule;
mod pagination;
mod portfolio;
mod rate_limit;
mod routing;
#[cfg(feature = "trading")]
mod trading;
pub mod types;

pub use auth::{
    ACCESS_KEY_HEADER, ACCESS_SIGNATURE_HEADER, ACCESS_TIMESTAMP_HEADER, Credentials, KEY_ID_ENV,
    KeyType, PRIVATE_KEY_PATH_ENV, SignedHeaders,
};
pub use client::{
    Client, ClientBuilder, DEMO_URL, MAX_ATTEMPTS, PRODUCTION_EXTERNAL_URL, PRODUCTION_URL, Request,
};
pub use endpoints::{BATCH_CANDLESTICK_TICKERS, MarketFilter, MarketStatus, Period};
pub use error::{KalshiError, Result};
pub use fee_schedule::{FeeScheduleRow, build_fee_schedule};
pub use portfolio::{FillFilter, OrderFilter, PositionFilter, SettlementFilter, SettlementStatus};
pub use rate_limit::{BATCH_CANCEL_COST, CF_BENCHMARKS_COST, DEFAULT_COST, GET_ORDER_COST, Tier};
pub use routing::{Cutoff, TradeId};

#[cfg(feature = "trading")]
pub use trading::{
    AmendOrder, AmendedOrder, BatchCancelOrder, BatchCancelResult, BatchCreateResult,
    BatchItemError, CancelledOrder, CreateOrderV2, CreatedSubaccount, DecreaseAmount,
    DecreasedOrder, DryRun, Environment, EventContractExitTrigger, ExitSlippage, ExitTrigger,
    ExitTriggerKind, ExitTriggerStatus, NewOrderGroup, Order, OrderBuilder, OrderGroup, OrderSide,
    SelfTradePrevention, SlippageUnit, SubaccountTransfer, TimeInForce, TradingClient,
    TradingClientBuilder, TriggeredLeg, WriteOutcome,
};
