//! Trading write surface, enabled with the `trading` cargo feature (off by default).
//!
//! A default build contains no write methods. [`TradingClient`] defaults to the demo
//! environment; production requires both [`Environment::Production`] and
//! [`TradingClientBuilder::allow_real_money`]. A global kill switch
//! ([`TradingClient::halt`]) makes every later write fail with
//! [`crate::KalshiError::TradingHalted`] without a network call, and [`TradingClientBuilder::dry_run`]
//! makes writes validate and sign but return a [`DryRun`] instead of sending.
//!
//! ```rust,no_run
//! # async fn run() -> furball::Result<()> {
//! use furball::{
//!     Credentials, NewOrder, OrderSide, SelfTradePrevention, TimeInForce, TradingClient,
//! };
//!
//! let client = TradingClient::builder()
//!     .credentials(Credentials::from_env()?)
//!     .build()?; // demo environment, real writes
//!
//! let order = NewOrder {
//!     ticker: "KXUPDOWN-25DEC".into(),
//!     side: OrderSide::Bid,
//!     count: 1.into(),
//!     price: "0.55".parse().expect("price"),
//!     time_in_force: TimeInForce::GoodTillCanceled,
//!     self_trade_prevention_type: SelfTradePrevention::TakerAtCross,
//!     client_order_id: Some("my-order-1".into()),
//!     post_only: None,
//!     reduce_only: None,
//! };
//! let _ = client.create_order(&order).await?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Credentials;
use crate::client::{Client, ClientBuilder, DEMO_URL, PRODUCTION_URL, Request};
use crate::error::{KalshiError, Result};
use crate::rate_limit::Tier;

/// Global kill switch shared by every [`TradingClient`] in the process.
static HALTED: AtomicBool = AtomicBool::new(false);

/// The environment a [`TradingClient`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    /// The demo exchange; no real money.
    Demo,
    /// The production exchange; real money. Building requires [`TradingClientBuilder::allow_real_money`].
    Production,
}

/// Builder for [`TradingClient`].
#[derive(Debug, Clone)]
pub struct TradingClientBuilder {
    environment: Environment,
    base_url: Option<String>,
    allow_real_money: bool,
    dry_run: bool,
    credentials: Option<Credentials>,
    inner: ClientBuilder,
}

impl Default for TradingClientBuilder {
    fn default() -> Self {
        Self {
            environment: Environment::Demo,
            base_url: None,
            allow_real_money: false,
            dry_run: false,
            credentials: None,
            inner: ClientBuilder::default(),
        }
    }
}

impl TradingClientBuilder {
    /// Starts a builder targeting the demo environment with the Basic tier.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the environment. Its base URL is used unless [`Self::base_url`] overrides it.
    pub fn environment(mut self, environment: Environment) -> Self {
        self.environment = environment;
        self
    }

    /// Overrides the base URL (for example a test server).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Required to build for [`Environment::Production`]; without it [`Self::build`] fails.
    pub fn allow_real_money(mut self) -> Self {
        self.allow_real_money = true;
        self
    }

    /// Makes every write validate and sign but return [`WriteOutcome::DryRun`] instead of sending.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// The account tier that sizes the rate buckets. Defaults to [`Tier::Basic`].
    pub fn tier(mut self, tier: Tier) -> Self {
        self.inner = self.inner.tier(tier);
        self
    }

    /// Credentials used to sign writes. Required.
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials.clone());
        self.inner = self.inner.credentials(credentials);
        self
    }

    /// Sets the first 429 retry delay; each further retry doubles it, plus jitter.
    pub fn backoff_base(mut self, backoff_base: Duration) -> Self {
        self.inner = self.inner.backoff_base(backoff_base);
        self
    }

    /// Validates the configuration and builds the client.
    ///
    /// Fails with [`KalshiError::TradingDisabled`] when the environment is production without
    /// [`Self::allow_real_money`], and with [`KalshiError::Auth`] when no credentials are set.
    pub fn build(self) -> Result<TradingClient> {
        if self.environment == Environment::Production && !self.allow_real_money {
            return Err(KalshiError::TradingDisabled(
                "production trading requires the explicit allow_real_money() opt-in".into(),
            ));
        }
        let credentials = self
            .credentials
            .ok_or_else(|| KalshiError::Auth("trading client requires credentials".into()))?;
        let base_url = self.base_url.unwrap_or_else(|| match self.environment {
            Environment::Demo => DEMO_URL.to_owned(),
            Environment::Production => PRODUCTION_URL.to_owned(),
        });
        let client = self
            .inner
            .base_url(base_url)
            .credentials(credentials)
            .build()?;
        Ok(TradingClient {
            client,
            environment: self.environment,
            dry_run: self.dry_run,
        })
    }
}

/// A client for trading writes. Exists only with the `trading` cargo feature.
///
/// Cheap to clone; clones share the rate limiter. The kill switch is process-global.
#[derive(Clone)]
pub struct TradingClient {
    client: Client,
    environment: Environment,
    dry_run: bool,
}

impl fmt::Debug for TradingClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TradingClient")
            .field("environment", &self.environment)
            .field("dry_run", &self.dry_run)
            .finish_non_exhaustive()
    }
}

impl TradingClient {
    /// Starts a [`TradingClientBuilder`] targeting the demo environment.
    pub fn builder() -> TradingClientBuilder {
        TradingClientBuilder::new()
    }

    /// The environment this client targets.
    pub fn environment(&self) -> Environment {
        self.environment
    }

    /// Whether writes are dry-run (validated and signed but never sent).
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Global kill switch: until [`Self::resume`], every write on every [`TradingClient`]
    /// returns [`KalshiError::TradingHalted`] without a network call.
    pub fn halt() {
        HALTED.store(true, Ordering::SeqCst);
    }

    /// Releases the global kill switch set by [`Self::halt`].
    pub fn resume() {
        HALTED.store(false, Ordering::SeqCst);
    }

    /// `POST /portfolio/events/orders`: create one order.
    ///
    /// With `client_order_id` set the request is marked idempotent (safe to retry); without it a
    /// failure after dispatch surfaces as [`KalshiError::Ambiguous`].
    pub async fn create_order(&self, order: &NewOrder) -> Result<WriteOutcome<Order>> {
        let mut request = Request::post("/portfolio/events/orders")
            .body(order)
            .authenticated();
        if order.client_order_id.is_some() {
            request = request.idempotent(true);
        }
        self.write(request, order.ticker.clone(), order.client_order_id.clone())
            .await
    }

    /// `DELETE /portfolio/events/orders/{order_id}`: cancel an order. Idempotent.
    pub async fn cancel_order(&self, order_id: &str) -> Result<WriteOutcome<CancelledOrder>> {
        self.write(
            Request::delete(format!("/portfolio/events/orders/{order_id}")).authenticated(),
            String::new(),
            None,
        )
        .await
    }

    async fn write<T: DeserializeOwned>(
        &self,
        request: Request,
        ticker: String,
        client_order_id: Option<String>,
    ) -> Result<WriteOutcome<T>> {
        if HALTED.load(Ordering::SeqCst) {
            return Err(KalshiError::TradingHalted);
        }
        tracing::info!(
            method = %request.method,
            path = %request.path,
            ticker = %ticker,
            client_order_id = ?client_order_id,
            dry_run = self.dry_run,
            "kalshi trading write"
        );
        if self.dry_run {
            let headers = self.client.signed_headers(&request)?;
            return Ok(WriteOutcome::DryRun(DryRun {
                method: request.method.as_str().to_owned(),
                path: request.path.clone(),
                cost: request.cost,
                timestamp_ms: headers.timestamp.parse().unwrap_or(0),
                body: request.body.clone().unwrap_or(serde_json::Value::Null),
            }));
        }
        let body = self.client.send_text(&request).await?;
        let decoded =
            serde_json::from_str(&body).map_err(|source| KalshiError::Decode { body, source })?;
        Ok(WriteOutcome::Sent(decoded))
    }
}

/// Outcome of a trading write.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOutcome<T> {
    /// The request was sent and the response decoded.
    Sent(T),
    /// Dry-run: the request was validated and signed but not sent.
    DryRun(DryRun),
}

/// Record of a dry-run write: validated and signed, never sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    /// HTTP method that would have been used.
    pub method: String,
    /// Path that would have been requested.
    pub path: String,
    /// Token cost the write would have charged.
    pub cost: u32,
    /// Millisecond timestamp used for signing.
    pub timestamp_ms: u128,
    /// JSON body that would have been sent.
    pub body: serde_json::Value,
}

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OrderSide {
    /// Buy.
    #[serde(rename = "bid")]
    Bid,
    /// Sell.
    #[serde(rename = "ask")]
    Ask,
}

/// Time in force for a new order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TimeInForce {
    /// Fill the whole order at once or cancel it.
    #[serde(rename = "fill_or_kill")]
    FillOrKill,
    /// Rest on the book until filled or canceled.
    #[serde(rename = "good_till_canceled")]
    GoodTillCanceled,
    /// Fill what is available immediately and cancel the rest.
    #[serde(rename = "immediate_or_cancel")]
    ImmediateOrCancel,
}

/// Self-trade prevention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SelfTradePrevention {
    /// This order takes liquidity across the spread.
    #[serde(rename = "taker_at_cross")]
    TakerAtCross,
    /// This order only joins the book.
    #[serde(rename = "maker")]
    Maker,
}

/// A new order for `POST /portfolio/events/orders`.
#[derive(Debug, Clone, Serialize)]
pub struct NewOrder {
    /// Market ticker.
    pub ticker: String,
    /// Side.
    pub side: OrderSide,
    /// Number of contracts, sent as a fixed-point string.
    #[serde(serialize_with = "decimal_as_string")]
    pub count: Decimal,
    /// Limit price in dollars, sent as a fixed-point string.
    #[serde(serialize_with = "decimal_as_string")]
    pub price: Decimal,
    /// Time in force.
    pub time_in_force: TimeInForce,
    /// Self-trade prevention mode.
    pub self_trade_prevention_type: SelfTradePrevention,
    /// Caller-chosen idempotency key; makes the create safe to retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    /// Post-only: joins the book or is rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    /// Reduce-only: capped at the current position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
}

fn decimal_as_string<S: serde::Serializer>(
    value: &Decimal,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Kalshi's order endpoints carry counts and prices as fixed-point strings.
fn decimal_from_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Decimal, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

fn optional_decimal_from_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Decimal>, D::Error> {
    let wrapper: Option<String> = Deserialize::deserialize(deserializer)?;
    wrapper
        .map(|text| text.parse().map_err(serde::de::Error::custom))
        .transpose()
}

/// A created order, as returned by `POST /portfolio/events/orders`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Order {
    /// Kalshi order id.
    pub order_id: String,
    /// The client order id, echoed back when supplied.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Contracts filled immediately.
    #[serde(deserialize_with = "decimal_from_string")]
    pub fill_count: Decimal,
    /// Contracts remaining after placement.
    #[serde(deserialize_with = "decimal_from_string")]
    pub remaining_count: Decimal,
    /// Volume-weighted average fill price, present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fill_price: Option<Decimal>,
    /// Volume-weighted average fee per contract, present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fee_paid: Option<Decimal>,
    /// Engine timestamp, Unix milliseconds.
    pub ts_ms: i64,
}

/// Response of `DELETE /portfolio/events/orders/{order_id}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CancelledOrder {
    /// Kalshi order id.
    pub order_id: String,
    /// The client order id, when present.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Contracts the cancel removed from the order.
    #[serde(deserialize_with = "decimal_from_string")]
    pub reduced_by: Decimal,
}
