//! Typed responses for Kalshi's market-data and portfolio endpoints.
//!
//! Money (`*_dollars`) and contract counts (`*_fp`) arrive as decimal strings and are held as
//! [`Decimal`]; nothing here uses `f64` for money. Unknown response fields are ignored.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

use crate::routing::TradeId;

/// Decodes a JSON number into a [`Decimal`] through its decimal text, never through `f64` arithmetic.
///
/// Kalshi sends fee multipliers as bare JSON numbers. `Decimal`'s own `Deserialize` rejects floats,
/// and its integer support depends on whether another crate turned on `serde_json/arbitrary_precision`.
fn decimal_from_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
    let number = serde_json::Number::deserialize(deserializer)?;
    number.to_string().parse().map_err(serde::de::Error::custom)
}

/// [`decimal_from_number`] for nullable fields.
fn optional_decimal_from_number<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    #[derive(Deserialize)]
    struct Wrapper(#[serde(deserialize_with = "decimal_from_number")] Decimal);
    Ok(Option::<Wrapper>::deserialize(deserializer)?.map(|wrapper| wrapper.0))
}

/// How Kalshi computes trading fees for a series or event override.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum FeeType {
    /// `quadratic`: taker fee only.
    Quadratic,
    /// `quadratic_with_maker_fees`.
    QuadraticWithMakerFees,
    /// `quadratic_with_combo_maker_fees`.
    QuadraticWithComboMakerFees,
    /// `flat`.
    Flat,
    /// A fee type this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for FeeType {
    fn from(value: String) -> Self {
        match value.as_str() {
            "quadratic" => Self::Quadratic,
            "quadratic_with_maker_fees" => Self::QuadraticWithMakerFees,
            "quadratic_with_combo_maker_fees" => Self::QuadraticWithComboMakerFees,
            "flat" => Self::Flat,
            _ => Self::Other(value),
        }
    }
}

impl From<FeeType> for String {
    fn from(value: FeeType) -> Self {
        match value {
            FeeType::Quadratic => "quadratic".into(),
            FeeType::QuadraticWithMakerFees => "quadratic_with_maker_fees".into(),
            FeeType::QuadraticWithComboMakerFees => "quadratic_with_combo_maker_fees".into(),
            FeeType::Flat => "flat".into(),
            FeeType::Other(other) => other,
        }
    }
}

/// A series: the template for recurring events, including its current fee.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Series {
    /// Series ticker.
    pub ticker: String,
    /// Display title.
    pub title: String,
    /// How often events recur.
    pub frequency: String,
    /// Category name.
    pub category: String,
    /// Fee type currently in force.
    pub fee_type: FeeType,
    /// Multiplier applied to the fee formula.
    #[serde(deserialize_with = "decimal_from_number")]
    pub fee_multiplier: Decimal,
    /// When the series metadata last changed.
    #[serde(default)]
    pub last_updated_ts: Option<DateTime<Utc>>,
    /// Total traded volume, present only when requested.
    #[serde(default)]
    pub volume_fp: Option<Decimal>,
}

/// Response of `GET /series/{series_ticker}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeriesResponse {
    /// The series.
    pub series: Series,
}

/// Response of `GET /series`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeriesListResponse {
    /// Matching series.
    pub series: Vec<Series>,
}

/// A scheduled change of a series' fee.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeriesFeeChange {
    /// Change identifier.
    pub id: String,
    /// Series the change applies to.
    pub series_ticker: String,
    /// Fee type from `scheduled_ts` onward.
    pub fee_type: FeeType,
    /// Fee multiplier from `scheduled_ts` onward.
    #[serde(deserialize_with = "decimal_from_number")]
    pub fee_multiplier: Decimal,
    /// When the change takes effect.
    pub scheduled_ts: DateTime<Utc>,
}

/// Response of `GET /series/fee_changes`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeriesFeeChangesResponse {
    /// Fee changes, in no guaranteed order.
    pub series_fee_change_arr: Vec<SeriesFeeChange>,
}

/// A scheduled fee override for one event, layered on the series fee.
///
/// A `None` override field inherits the series value; both `None` clears the override.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventFeeChange {
    /// Change identifier.
    pub id: String,
    /// Event the override applies to.
    pub event_ticker: String,
    /// Series that owns the event.
    pub series_ticker: String,
    /// Fee type override, if any.
    pub fee_type_override: Option<FeeType>,
    /// Fee multiplier override, if any.
    #[serde(deserialize_with = "optional_decimal_from_number")]
    pub fee_multiplier_override: Option<Decimal>,
    /// When the override takes effect.
    pub scheduled_ts: DateTime<Utc>,
}

/// An event: one real-world occurrence with one or more markets.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Event {
    /// Event ticker.
    pub event_ticker: String,
    /// Series the event belongs to.
    pub series_ticker: String,
    /// Display title.
    pub title: String,
    /// Display subtitle.
    pub sub_title: String,
    /// Whether exactly one market can resolve yes.
    pub mutually_exclusive: bool,
    /// Category name.
    #[serde(default)]
    pub category: Option<String>,
    /// When the event's outcome is measured, if scheduled.
    #[serde(default)]
    pub strike_date: Option<DateTime<Utc>>,
    /// Nested markets, present only when requested.
    #[serde(default)]
    pub markets: Option<Vec<Market>>,
    /// Fee type override currently on the event.
    #[serde(default)]
    pub fee_type_override: Option<FeeType>,
    /// Fee multiplier override currently on the event.
    #[serde(default, deserialize_with = "optional_decimal_from_number")]
    pub fee_multiplier_override: Option<Decimal>,
}

/// Response of `GET /events/{event_ticker}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventResponse {
    /// The event.
    pub event: Event,
    /// Its markets.
    pub markets: Vec<Market>,
}

/// A binary or scalar market.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Market {
    /// Market ticker.
    pub ticker: String,
    /// Event the market belongs to.
    pub event_ticker: String,
    /// `binary` or `scalar`.
    pub market_type: String,
    /// Lifecycle status (`active`, `finalized`, ...).
    pub status: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// When trading opens.
    pub open_time: DateTime<Utc>,
    /// When trading closes.
    pub close_time: DateTime<Utc>,
    /// Latest time the market can expire.
    pub latest_expiration_time: DateTime<Utc>,
    /// When the market settled, if it has.
    #[serde(default)]
    pub settlement_ts: Option<DateTime<Utc>>,
    /// `yes`, `no`, `scalar`, or empty before resolution.
    pub result: String,
    /// Settlement value of one yes contract in dollars, once settled.
    #[serde(default)]
    pub settlement_value_dollars: Option<Decimal>,
    /// Best yes bid.
    pub yes_bid_dollars: Decimal,
    /// Best yes ask.
    pub yes_ask_dollars: Decimal,
    /// Last traded yes price.
    pub last_price_dollars: Decimal,
    /// Contracts traded.
    pub volume_fp: Decimal,
    /// Contracts traded in the last 24 hours.
    pub volume_24h_fp: Decimal,
    /// Open interest in contracts.
    pub open_interest_fp: Decimal,
}

/// Response of `GET /markets/{ticker}` and `GET /historical/markets/{ticker}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MarketResponse {
    /// The market.
    pub market: Market,
}

/// Open, high, low and close of an offer price over one candlestick period, in dollars.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BidAskCandle {
    /// Price at the start of the period.
    #[serde(alias = "open")]
    pub open_dollars: Decimal,
    /// Lowest price during the period.
    #[serde(alias = "low")]
    pub low_dollars: Decimal,
    /// Highest price during the period.
    #[serde(alias = "high")]
    pub high_dollars: Decimal,
    /// Price at the end of the period.
    #[serde(alias = "close")]
    pub close_dollars: Decimal,
}

/// Trade price statistics over one candlestick period; every field is `None` when nothing traded.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PriceCandle {
    /// First trade price.
    #[serde(default, alias = "open")]
    pub open_dollars: Option<Decimal>,
    /// Lowest trade price.
    #[serde(default, alias = "low")]
    pub low_dollars: Option<Decimal>,
    /// Highest trade price.
    #[serde(default, alias = "high")]
    pub high_dollars: Option<Decimal>,
    /// Last trade price.
    #[serde(default, alias = "close")]
    pub close_dollars: Option<Decimal>,
    /// Volume-weighted average trade price.
    #[serde(default, alias = "mean")]
    pub mean_dollars: Option<Decimal>,
    /// Last trade price before the period.
    #[serde(default, alias = "previous")]
    pub previous_dollars: Option<Decimal>,
}

/// One candlestick. Live and historical endpoints name some fields differently; both decode here.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Candlestick {
    /// Inclusive end of the period, Unix seconds.
    pub end_period_ts: i64,
    /// Yes bid statistics.
    pub yes_bid: BidAskCandle,
    /// Yes ask statistics.
    pub yes_ask: BidAskCandle,
    /// Trade price statistics.
    pub price: PriceCandle,
    /// Contracts traded during the period.
    #[serde(alias = "volume")]
    pub volume_fp: Decimal,
    /// Open interest at the end of the period.
    #[serde(alias = "open_interest")]
    pub open_interest_fp: Decimal,
}

/// Response of the single-market candlestick endpoints, live and historical.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CandlesticksResponse {
    /// Market ticker.
    pub ticker: String,
    /// Candlesticks, oldest first.
    pub candlesticks: Vec<Candlestick>,
}

/// One market's entry in a batch candlestick response.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MarketCandlesticks {
    /// Market ticker.
    pub market_ticker: String,
    /// Candlesticks, oldest first.
    pub candlesticks: Vec<Candlestick>,
}

/// Response of `GET /markets/candlesticks`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BatchCandlesticksResponse {
    /// One entry per requested market.
    pub markets: Vec<MarketCandlesticks>,
}

/// Which side the taker was positioned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeSide {
    /// Taker bought yes or sold no.
    Yes,
    /// Taker bought no or sold yes.
    No,
}

/// A completed trade.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Trade {
    /// Unique trade identifier.
    pub trade_id: String,
    /// Market ticker.
    pub ticker: String,
    /// Contracts traded.
    pub count_fp: Decimal,
    /// Yes price in dollars.
    pub yes_price_dollars: Decimal,
    /// No price in dollars.
    pub no_price_dollars: Decimal,
    /// Taker's outcome side.
    pub taker_outcome_side: OutcomeSide,
    /// When the trade executed.
    pub created_time: DateTime<Utc>,
    /// Whether the trade was matched off-book.
    pub is_block_trade: bool,
}

impl TradeId for Trade {
    fn trade_id(&self) -> &str {
        &self.trade_id
    }
}

/// One price level: price in dollars and contracts resting there.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(from = "(Decimal, Decimal)")]
pub struct PriceLevel {
    /// Price in dollars.
    pub price_dollars: Decimal,
    /// Contracts resting at the price.
    pub count_fp: Decimal,
}

impl From<(Decimal, Decimal)> for PriceLevel {
    fn from((price_dollars, count_fp): (Decimal, Decimal)) -> Self {
        Self {
            price_dollars,
            count_fp,
        }
    }
}

/// Resting bids on both sides, best price last as Kalshi returns them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Orderbook {
    /// Yes bids.
    pub yes_dollars: Vec<PriceLevel>,
    /// No bids.
    pub no_dollars: Vec<PriceLevel>,
}

/// Response of `GET /markets/{ticker}/orderbook`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderbookResponse {
    /// The current snapshot.
    pub orderbook_fp: Orderbook,
}

/// Response of the CF Benchmarks passthrough: the upstream payload under `data`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CfBenchmarksResponse {
    /// Raw CF Benchmarks payload.
    pub data: serde_json::Value,
}

/// Balance and portfolio value of one account, from `GET /portfolio/balance`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Balance {
    /// Available balance in cents, across all exchange shards unless one was requested.
    pub balance: i64,
    /// Available balance in dollars.
    pub balance_dollars: Decimal,
    /// Portfolio value in cents.
    pub portfolio_value: i64,
    /// Unix timestamp of the last balance update.
    pub updated_ts: i64,
    /// Balance per exchange shard; omitted for subaccount-restricted API keys.
    #[serde(default)]
    pub balance_breakdown: Option<Vec<IndexedBalance>>,
}

/// Balance of one exchange shard.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IndexedBalance {
    /// Exchange shard the balance is held on.
    pub exchange_index: u32,
    /// Balance in dollars.
    pub balance: Decimal,
}

/// Balance of one subaccount on one exchange shard.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SubaccountBalance {
    /// Subaccount number, 0 for the primary account.
    pub subaccount_number: u32,
    /// Exchange shard the balance is held on.
    pub exchange_index: u32,
    /// Balance in dollars.
    pub balance: Decimal,
    /// Unix timestamp of the last balance update.
    pub updated_ts: i64,
}

/// Response of `GET /portfolio/subaccounts/balances`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SubaccountBalancesResponse {
    /// One entry per subaccount and exchange shard, including the primary account.
    pub subaccount_balances: Vec<SubaccountBalance>,
}

/// Market-level position from `GET /portfolio/positions`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MarketPosition {
    /// Market ticker.
    pub ticker: String,
    /// Exchange shard.
    pub exchange_index: u32,
    /// Total spent trading this market, in dollars; not the remaining position's cost.
    pub total_traded_dollars: Decimal,
    /// Signed net contracts: positive for YES, negative for NO, zero for flat.
    pub position_fp: Decimal,
    /// Cost basis of the remaining position, in dollars; not its current market value.
    pub market_exposure_dollars: Decimal,
    /// Locked-in profit and loss, in dollars.
    pub realized_pnl_dollars: Decimal,
    /// Fees paid on fills, in dollars.
    pub fees_paid_dollars: Decimal,
    /// When the position last changed.
    pub last_updated_ts: DateTime<Utc>,
    /// Whether netting applies to this position's event.
    pub netting_enabled: bool,
}

/// Event-level aggregate position from `GET /portfolio/positions`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventPosition {
    /// Event ticker.
    pub event_ticker: String,
    /// Total spent on this event, in dollars.
    pub total_cost_dollars: Decimal,
    /// Total shares traded on the event, YES and NO combined.
    pub total_cost_shares_fp: Decimal,
    /// Cost of the aggregate event position, in dollars.
    pub event_exposure_dollars: Decimal,
    /// Locked-in profit and loss, in dollars.
    pub realized_pnl_dollars: Decimal,
    /// Fees paid on fills, in dollars.
    pub fees_paid_dollars: Decimal,
}

/// One page of `GET /portfolio/positions`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PositionsPage {
    /// Market positions on this page.
    pub market_positions: Vec<MarketPosition>,
    /// Event positions on this page.
    pub event_positions: Vec<EventPosition>,
    /// Cursor for the next page; empty or absent when the list is exhausted.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Market and event positions returned by `GET /portfolio/positions`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Positions {
    /// Market-level positions.
    pub market_positions: Vec<MarketPosition>,
    /// Event-level positions.
    pub event_positions: Vec<EventPosition>,
}

impl Positions {
    /// Total number of positions, market plus event.
    pub fn len(&self) -> usize {
        self.market_positions.len() + self.event_positions.len()
    }

    /// Whether the result holds no positions at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn extend(&mut self, market: Vec<MarketPosition>, event: Vec<EventPosition>) {
        self.market_positions.extend(market);
        self.event_positions.extend(event);
    }
}

/// Outcome of a settled market.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum MarketResult {
    /// Resolved yes.
    Yes,
    /// Resolved no.
    No,
    /// Scalar market settled at a value.
    Scalar,
    /// A result this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for MarketResult {
    fn from(value: String) -> Self {
        match value.as_str() {
            "yes" => Self::Yes,
            "no" => Self::No,
            "scalar" => Self::Scalar,
            _ => Self::Other(value),
        }
    }
}

/// A settlement record from `GET /portfolio/settlements`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Settlement {
    /// Ticker of the settled market.
    pub ticker: String,
    /// Exchange shard.
    pub exchange_index: u32,
    /// Ticker of the market's event.
    pub event_ticker: String,
    /// Settlement outcome.
    pub market_result: MarketResult,
    /// YES contracts owned at settlement.
    pub yes_count_fp: Decimal,
    /// Cost basis of the YES contracts, in dollars.
    pub yes_total_cost_dollars: Decimal,
    /// NO contracts owned at settlement.
    pub no_count_fp: Decimal,
    /// Cost basis of the NO contracts, in dollars.
    pub no_total_cost_dollars: Decimal,
    /// Revenue from this settlement, in cents; winning contracts pay out 100 cents each.
    pub revenue: i64,
    /// When the market settled and payouts were processed.
    pub settled_time: DateTime<Utc>,
    /// Fees paid, in dollars.
    pub fee_cost: Decimal,
    /// Payout of a single yes contract, in cents, if the market paid out.
    #[serde(default)]
    pub value: Option<i64>,
}
