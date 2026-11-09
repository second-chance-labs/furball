//! Typed responses for Kalshi's market-data endpoints.
//!
//! Money (`*_dollars`) and contract counts (`*_fp`) arrive as decimal strings and are held as
//! [`Decimal`]; nothing here uses `f64` for money. Unknown response fields are ignored.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

use crate::rate_limit::Tier;
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

/// Response of `GET /account/limits`: the account's usage tier, bucket limits, and grants.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountLimits {
    /// Effective usage tier.
    pub usage_tier: Tier,
    /// Read bucket limits.
    pub read: BucketLimit,
    /// Write bucket limits.
    pub write: BucketLimit,
    /// Grants that raise the usage level of an exchange instance.
    pub grants: Vec<ApiUsageLevelGrant>,
}

/// The configured limits of one rate bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct BucketLimit {
    /// Tokens added per second.
    pub refill_rate: u32,
    /// Maximum tokens the bucket holds.
    pub bucket_capacity: u32,
}

/// A grant raising the usage level of one exchange instance.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ApiUsageLevelGrant {
    /// Exchange instance the grant applies to.
    pub exchange_instance: ExchangeInstance,
    /// Granted usage level.
    pub level: Tier,
    /// How the grant was earned.
    pub source: GrantSource,
    /// Expiry, Unix seconds; `None` for permanent grants.
    #[serde(default)]
    pub expires_ts: Option<i64>,
}

/// Exchange instance a usage-level grant applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ExchangeInstance {
    /// Event contracts.
    EventContract,
    /// Margined trading.
    Margined,
    /// An exchange instance this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for ExchangeInstance {
    fn from(value: String) -> Self {
        match value.as_str() {
            "event_contract" => Self::EventContract,
            "margined" => Self::Margined,
            _ => Self::Other(value),
        }
    }
}

impl From<ExchangeInstance> for String {
    fn from(value: ExchangeInstance) -> Self {
        match value {
            ExchangeInstance::EventContract => "event_contract".into(),
            ExchangeInstance::Margined => "margined".into(),
            ExchangeInstance::Other(other) => other,
        }
    }
}

/// How a usage-level grant was earned.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum GrantSource {
    /// Earned through trading volume.
    Volume,
    /// Applied manually by Kalshi.
    Manual,
    /// A source this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for GrantSource {
    fn from(value: String) -> Self {
        match value.as_str() {
            "volume" => Self::Volume,
            "manual" => Self::Manual,
            _ => Self::Other(value),
        }
    }
}

impl From<GrantSource> for String {
    fn from(value: GrantSource) -> Self {
        match value {
            GrantSource::Volume => "volume".into(),
            GrantSource::Manual => "manual".into(),
            GrantSource::Other(other) => other,
        }
    }
}

/// Response of `GET /account/endpoint_costs`: the default cost plus every endpoint that deviates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EndpointCosts {
    /// Token cost of an endpoint not listed in [`Self::endpoint_costs`].
    pub default_cost: u32,
    /// Endpoints whose cost differs from [`Self::default_cost`].
    pub endpoint_costs: Vec<EndpointCost>,
}

/// The configured cost of one endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EndpointCost {
    /// HTTP method, for example `GET` or `POST`.
    pub method: String,
    /// Path relative to the base URL, for example `/portfolio/orders`.
    pub path: String,
    /// Token cost.
    pub cost: u32,
}
