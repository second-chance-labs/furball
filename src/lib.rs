#![doc = include_str!("../README.md")]
//! Read-only transport client for Kalshi's Trade API v2.
//!
//! - [`Client`] signs requests ([`Credentials`]), throttles them with a token bucket sized by [`Tier`],
//!   and retries HTTP 429 with exponential backoff.
//! - [`Client::paginate`] follows cursors as a lazy stream.
//! - [`Client::market`], [`Client::candlesticks`] and [`Client::trades`] hide the split between live
//!   and historical endpoints using the cached [`Cutoff`].
//! - Authenticated portfolio reads: [`Client::balance`] (its own per-second bucket),
//!   [`Client::positions`], [`Client::settlements`] and [`Client::subaccount_balances`].
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
pub mod types;

pub use auth::{
    ACCESS_KEY_HEADER, ACCESS_SIGNATURE_HEADER, ACCESS_TIMESTAMP_HEADER, Credentials, KEY_ID_ENV,
    PRIVATE_KEY_PATH_ENV, SignedHeaders,
};
pub use client::{
    Client, ClientBuilder, DEMO_URL, MAX_ATTEMPTS, PRODUCTION_EXTERNAL_URL, PRODUCTION_URL, Request,
};
pub use endpoints::{BATCH_CANDLESTICK_TICKERS, MarketFilter, MarketStatus, Period};
pub use error::{KalshiError, Result};
pub use fee_schedule::{FeeScheduleRow, build_fee_schedule};
pub use portfolio::{PositionFilter, SettlementFilter, SettlementStatus};
pub use rate_limit::{CF_BENCHMARKS_COST, DEFAULT_COST, Tier};
pub use routing::{Cutoff, TradeId};
