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
//! - The `testing` feature (which enables `trading`) adds an in-process fake exchange
//!   (`testing::FakeExchange`): price-and-time priority matching for limit orders
//!   (good-till-canceled, immediate-or-cancel, fill-or-kill), fees from [`FeeCalculator`],
//!   balance, position and settlement tracking, and a test-controlled clock, market data
//!   and settlement.
//! - Authenticated portfolio reads: [`Client::balance`] (its own per-second bucket),
//!   [`Client::positions`], [`Client::settlements`] and [`Client::subaccount_balances`].
//! - Authenticated order and fill reads, live and archived: [`Client::get_order`],
//!   [`Client::orders`], [`Client::fills`], [`Client::position_history`] and the queue-position
//!   endpoints, split at the cached [`Cutoff`] and merged like trades.
//! - Validated market quantities without network calls: [`Price`] (dollars, strictly between 0
//!   and 1, checkable against the market's tick size via [`Price::validate`]) and [`Count`]
//!   (contracts, non-negative), plus cents/dollars conversions on [`Price`].
//! - Order book helpers on [`types::Orderbook`]: best bid and ask on both sides (asks derived
//!   as `1 -` the opposite bid), spread, mid price, depth at N levels, and taker fill cost
//!   ([`FillCost`]) via [`types::Orderbook::cost_to_fill`].
//! - Position math from fills: the settlement payoff of yes and no positions on binary
//!   and scalar markets ([`payoff_at_settlement`]), realized and unrealized profit and
//!   loss with fees subtracted ([`realized_pnl`], [`unrealized_pnl`]), and the
//!   volume-weighted average entry price ([`average_entry_price`]).
//! - Fee calculator ([`FeeCalculator`]): the quadratic fee formulas with the published
//!   coefficients ([`FeeCoefficients`], overridable via environment variables), Kalshi's
//!   rounding pipeline (trade fee rounded up to $0.000001, balance alignment, per-order
//!   rebates in [`FeeAccumulator`]), and [`fee_in_force`] to resolve the fee type and
//!   multiplier from a [`build_fee_schedule`] at the fill time.
//! - RFQ and quote reads for negotiated trades: [`Client::get_rfq`], [`Client::list_rfqs`],
//!   [`Client::get_quote`] and [`Client::list_quotes`]; their writes (create, delete, accept,
//!   confirm) live on the trading surface, along with the RFQ batch writes that quote several
//!   combos as one all-or-nothing unit.
//! - Block trade proposals for large negotiated trades: the read is
//!   [`Client::list_block_trade_proposals`], and the writes (propose, cancel, accept, reject)
//!   live on the trading surface.
//!
//! ```text
//! caller -> routing (cutoff cache) -> pagination -> send_text (bucket -> sign -> HTTP -> 429 retry)
//! ```

mod auth;
mod client;
mod communications;
mod endpoints;
mod error;
mod fee_schedule;
mod fees;
mod orderbook;
mod pagination;
mod pnl;
mod portfolio;
mod pricing;
mod rate_limit;
mod routing;
#[cfg(feature = "testing")]
pub mod testing;
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
pub use communications::{
    AcceptedSide, BlockTradeProposal, BlockTradeProposalFilter, BlockTradeProposalStatus,
    MveSelectedLeg, Quote, QuoteFilter, QuoteStatus, Rfq, RfqFilter, RfqStatus,
};
pub use endpoints::{BATCH_CANDLESTICK_TICKERS, MarketFilter, MarketStatus, Period};
pub use error::{KalshiError, Result};
pub use fee_schedule::{FeeScheduleRow, build_fee_schedule};
pub use fees::{
    BalancePrecision, FEE_COMBO_MAKER_ENV, FEE_MAKER_ENV, FEE_TAKER_ENV, FeeAccumulator,
    FeeBreakdown, FeeCalculator, FeeCoefficientError, FeeCoefficients, FeeError, FeeSide,
    ScheduledFee, fee_in_force,
};
pub use orderbook::{BookError, FillCost};
pub use pnl::{
    PnlError, SettlementError, average_entry_price, payoff_at_settlement, realized_pnl,
    unrealized_pnl,
};
pub use portfolio::{FillFilter, OrderFilter, PositionFilter, SettlementFilter, SettlementStatus};
pub use pricing::{Count, CountError, Price, PriceError};
pub use rate_limit::{BATCH_CANCEL_COST, CF_BENCHMARKS_COST, DEFAULT_COST, GET_ORDER_COST, Tier};
pub use routing::{Cutoff, TradeId};

#[cfg(feature = "trading")]
pub use communications::{
    AcceptBlockTradeProposal, AcceptRfqBatch, AcceptedRfqBatch, CreatedBlockTradeProposal,
    CreatedQuote, CreatedRfq, CreatedRfqBatchQuote, NewQuote, NewRfq, NewRfqBatch,
    NewRfqBatchQuote, ProposeBlockTrade, RfqBatch, RfqBatchAcceptLimit, RfqBatchChild,
    RfqBatchQuoteEntry, RfqBatchSize, RfqBatchStatus, RfqChildSize, RfqSize, TickerPair,
};
#[cfg(feature = "trading")]
pub use trading::{
    AmendOrder, AmendedOrder, BatchCancelOrder, BatchCancelResult, BatchCreateResult,
    BatchItemError, CancelledOrder, CreateOrderV2, CreatedSubaccount, DecreaseAmount,
    DecreasedOrder, DryRun, Environment, EventContractExitTrigger, ExitSlippage, ExitTrigger,
    ExitTriggerKind, ExitTriggerStatus, NewOrderGroup, Order, OrderBuilder, OrderGroup, OrderSide,
    PLACE_AND_CONFIRM_MAX_TRIES, SelfTradePrevention, SlippageUnit, SubaccountTransfer,
    TimeInForce, TradingClient, TradingClientBuilder, TriggeredLeg, WriteOutcome,
};
