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
//!     Credentials, OrderBuilder, OrderSide, SelfTradePrevention, TimeInForce, TradingClient,
//! };
//!
//! let client = TradingClient::builder()
//!     .credentials(Credentials::from_env()?)
//!     .build()?; // demo environment, real writes
//!
//! let order = OrderBuilder::new(
//!     "KXUPDOWN-25DEC",
//!     OrderSide::Bid,
//!     1.into(),
//!     "0.55".parse().expect("price"),
//!     TimeInForce::GoodTillCanceled,
//!     SelfTradePrevention::TakerAtCross,
//! )
//! .build();
//! let _ = client.create_order(&order).await?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use rand::Rng;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Credentials;
use crate::client::{Client, ClientBuilder, DEMO_URL, PRODUCTION_URL, Request};
use crate::error::{KalshiError, Result};
use crate::rate_limit::{BATCH_CANCEL_COST, DEFAULT_COST, Tier};
use crate::types::{decimal_as_string, decimal_from_string, optional_decimal_from_string};

/// Global kill switch shared by every [`TradingClient`] in the process.
static HALTED: AtomicBool = AtomicBool::new(false);

/// How many times [`TradingClient::place_and_confirm`] sends the order before giving up.
///
/// Every attempt is one send followed, after an ambiguous failure, by one lookup; when the
/// final attempt's lookup finds no order the call fails with an error naming the
/// `client_order_id` to reconcile.
pub const PLACE_AND_CONFIRM_MAX_TRIES: u32 = 3;

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
    pub(crate) client: Client,
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
    /// failure after dispatch surfaces as [`KalshiError::Ambiguous`]. [`OrderBuilder`] supplies a
    /// generated id unless told not to.
    pub async fn create_order(&self, order: &CreateOrderV2) -> Result<WriteOutcome<Order>> {
        let mut request = Request::post("/portfolio/events/orders")
            .body(order)
            .authenticated();
        if order.client_order_id.is_some() {
            request = request.idempotent(true);
        }
        self.write(request, order.ticker.clone(), order.client_order_id.clone())
            .await
    }

    /// Places an order and confirms its outcome, so an ambiguous failure can never create a
    /// duplicate order.
    ///
    /// The order is sent with a `client_order_id`: the caller's when set, otherwise one
    /// generated by this helper. Every attempt reuses that id and Kalshi deduplicates orders on
    /// it, so one call can create at most one order no matter how many times it sends.
    ///
    /// The send is deliberately not marked idempotent: a timeout, connection failure, or HTTP
    /// 5xx must surface as [`KalshiError::Ambiguous`] so this helper reconciles before any
    /// retry, instead of the caller retrying blindly. When that happens it looks the order up
    /// by `client_order_id` on `GET /portfolio/orders`: if the order exists it is returned; if
    /// it does not, the send is retried with the same `client_order_id`. Any other error (a
    /// rejected order, a rate limit, the kill switch) is returned as-is. After
    /// [`PLACE_AND_CONFIRM_MAX_TRIES`] ambiguous attempts the call fails with a
    /// [`KalshiError::Ambiguous`] naming the `client_order_id` to reconcile.
    ///
    /// An order found by the lookup is mapped onto the create-response shape: the average-fill
    /// fields are `None` and `ts_ms` is the last update (or creation) time in Unix
    /// milliseconds.
    pub async fn place_and_confirm(&self, order: &CreateOrderV2) -> Result<WriteOutcome<Order>> {
        let client_order_id = order
            .client_order_id
            .clone()
            .unwrap_or_else(random_client_order_id);
        let mut to_send = order.clone();
        to_send.client_order_id = Some(client_order_id.clone());
        let mut tries = 0u32;
        loop {
            tries += 1;
            let request = Request::post("/portfolio/events/orders")
                .body(&to_send)
                .authenticated();
            match self
                .write(
                    request,
                    to_send.ticker.clone(),
                    Some(client_order_id.clone()),
                )
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(KalshiError::Ambiguous(reason)) => {
                    if let Some(found) = self
                        .find_by_client_order_id(&to_send.ticker, &client_order_id)
                        .await?
                    {
                        return Ok(WriteOutcome::Sent(found));
                    }
                    if tries >= PLACE_AND_CONFIRM_MAX_TRIES {
                        return Err(KalshiError::Ambiguous(format!(
                            "order with client_order_id {client_order_id} was not confirmed \
                             after {PLACE_AND_CONFIRM_MAX_TRIES} attempts (last failure: \
                             {reason}); it may still exist - look it up or cancel it before \
                             placing a new order"
                        )));
                    }
                }
                Err(error) => return Err(error),
            }
        }
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

    /// `DELETE /portfolio/events/orders`: cancel every resting order on one subaccount.
    ///
    /// The subaccount number (0 for the primary account) is a required argument so a caller
    /// cannot cancel across every subaccount without naming one. Returns 204 with no body.
    pub async fn cancel_all(&self, subaccount: u32) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete("/portfolio/events/orders")
                .query("subaccount", subaccount)
                .authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `POST /portfolio/events/orders/{order_id}/amend`: change the price, size, and/or
    /// expiration of a resting order. Not idempotent: a failure after dispatch surfaces as
    /// [`KalshiError::Ambiguous`].
    pub async fn amend_order(
        &self,
        order_id: &str,
        amend: &AmendOrder,
    ) -> Result<WriteOutcome<AmendedOrder>> {
        let request = Request::post(format!("/portfolio/events/orders/{order_id}/amend"))
            .body(amend)
            .authenticated();
        self.write(request, amend.ticker.clone(), None).await
    }

    /// `POST /portfolio/events/orders/{order_id}/decrease`: reduce a resting order's remaining
    /// count, either by an amount ([`DecreaseAmount::By`]) or to an amount
    /// ([`DecreaseAmount::To`]). Not idempotent: a failure after dispatch surfaces as
    /// [`KalshiError::Ambiguous`].
    pub async fn decrease_order(
        &self,
        order_id: &str,
        amount: &DecreaseAmount,
    ) -> Result<WriteOutcome<DecreasedOrder>> {
        let request = Request::post(format!("/portfolio/events/orders/{order_id}/decrease"))
            .body(amount)
            .authenticated();
        self.write(request, String::new(), None).await
    }

    /// `POST /portfolio/events/orders/batched`: create several orders in one request.
    ///
    /// Billed [`DEFAULT_COST`] tokens per item: the whole batch must fit in the Write bucket's
    /// capacity, otherwise the request is rejected locally with
    /// [`KalshiError::CostExceedsBucket`] before anything is sent. The response has one
    /// [`BatchCreateResult`] per item; failed items carry `error` while successful items are
    /// reported normally. Idempotent (safe to retry) only when every item has a
    /// `client_order_id`; otherwise a failure after dispatch surfaces as
    /// [`KalshiError::Ambiguous`].
    pub async fn batch_create_orders(
        &self,
        orders: &[CreateOrderV2],
    ) -> Result<WriteOutcome<Vec<BatchCreateResult>>> {
        let mut request = Request::post("/portfolio/events/orders/batched")
            .body(BatchCreateBody {
                orders: orders.to_vec(),
            })
            .authenticated()
            .cost(batch_cost(orders.len(), DEFAULT_COST));
        if orders.iter().all(|order| order.client_order_id.is_some()) {
            request = request.idempotent(true);
        }
        let ticker = orders
            .first()
            .map(|order| order.ticker.clone())
            .unwrap_or_default();
        let outcome: WriteOutcome<BatchCreateResponse> = self.write(request, ticker, None).await?;
        Ok(match outcome {
            WriteOutcome::Sent(response) => WriteOutcome::Sent(response.orders),
            WriteOutcome::DryRun(dry) => WriteOutcome::DryRun(dry),
        })
    }

    /// `DELETE /portfolio/events/orders/batched`: cancel several orders in one request.
    ///
    /// Billed [`BATCH_CANCEL_COST`] tokens per item: the whole batch must fit in the Write
    /// bucket's capacity, otherwise the request is rejected locally with
    /// [`KalshiError::CostExceedsBucket`] before anything is sent. The response has one
    /// [`BatchCancelResult`] per item; failed items carry `error` while successful items are
    /// reported normally.
    pub async fn batch_cancel_orders(
        &self,
        orders: &[BatchCancelOrder],
    ) -> Result<WriteOutcome<Vec<BatchCancelResult>>> {
        let request = Request::delete("/portfolio/events/orders/batched")
            .body(BatchCancelBody {
                orders: orders.to_vec(),
            })
            .authenticated()
            .cost(batch_cost(orders.len(), BATCH_CANCEL_COST));
        let outcome: WriteOutcome<BatchCancelResponse> =
            self.write(request, String::new(), None).await?;
        Ok(match outcome {
            WriteOutcome::Sent(response) => WriteOutcome::Sent(response.orders),
            WriteOutcome::DryRun(dry) => WriteOutcome::DryRun(dry),
        })
    }

    /// `POST /portfolio/order_groups/create`: create an order group with a contracts limit
    /// measured over a rolling 15-second window. When the limit is hit, all orders in the group
    /// are canceled and no new orders can be placed until the group is reset.
    pub async fn create_order_group(
        &self,
        group: &NewOrderGroup,
    ) -> Result<WriteOutcome<OrderGroup>> {
        let request = Request::post("/portfolio/order_groups/create")
            .body(group)
            .authenticated();
        self.write(request, String::new(), None).await
    }

    /// `DELETE /portfolio/order_groups/{order_group_id}`: delete an order group and cancel all
    /// orders within it. Idempotent.
    pub async fn delete_order_group(&self, order_group_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!("/portfolio/order_groups/{order_group_id}")).authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /portfolio/order_groups/{order_group_id}/reset`: reset the group's rolling limit so
    /// new orders can be placed again. Idempotent.
    pub async fn reset_order_group(&self, order_group_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::put(format!("/portfolio/order_groups/{order_group_id}/reset")).authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /portfolio/order_groups/{order_group_id}/trigger`: cancel all of the group's orders
    /// immediately, as if the limit had been hit. Idempotent.
    pub async fn trigger_order_group(&self, order_group_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::put(format!("/portfolio/order_groups/{order_group_id}/trigger"))
                .authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /portfolio/order_groups/{order_group_id}/limit`: change the group's rolling contracts
    /// limit. Idempotent.
    pub async fn update_order_group_limit(
        &self,
        order_group_id: &str,
        contracts_limit: &Decimal,
    ) -> Result<WriteOutcome<()>> {
        let request = Request::put(format!("/portfolio/order_groups/{order_group_id}/limit"))
            .body(UpdateOrderGroupLimitBody {
                contracts_limit_fp: *contracts_limit,
            })
            .authenticated();
        self.write_empty(request, String::new(), None).await
    }

    /// `POST /portfolio/events/orders/with_exit_trigger`: create an order with an attached
    /// stop-loss/take-profit trigger in one call. The response is the created order. Idempotent
    /// (safe to retry) only when the order has a `client_order_id`; otherwise a failure after
    /// dispatch surfaces as [`KalshiError::Ambiguous`].
    pub async fn create_order_with_exit_trigger(
        &self,
        order: &CreateOrderV2,
        exit_trigger: &ExitTrigger,
    ) -> Result<WriteOutcome<Order>> {
        let mut request = Request::post("/portfolio/events/orders/with_exit_trigger")
            .body(CreateOrderWithExitTriggerBody {
                order,
                exit_trigger,
            })
            .authenticated();
        if order.client_order_id.is_some() {
            request = request.idempotent(true);
        }
        self.write(request, order.ticker.clone(), order.client_order_id.clone())
            .await
    }

    /// `PUT /portfolio/positions/{ticker}/exit_trigger`: set the position exit trigger for one
    /// market. Idempotent.
    pub async fn set_position_exit_trigger(
        &self,
        ticker: &str,
        trigger: &ExitTrigger,
    ) -> Result<WriteOutcome<EventContractExitTrigger>> {
        let request = Request::put(format!("/portfolio/positions/{ticker}/exit_trigger"))
            .body(trigger)
            .authenticated();
        self.write(request, ticker.to_owned(), None).await
    }

    /// `DELETE /portfolio/positions/{ticker}/exit_trigger`: remove the position exit trigger for
    /// one market. Idempotent.
    pub async fn delete_position_exit_trigger(&self, ticker: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!("/portfolio/positions/{ticker}/exit_trigger")).authenticated(),
            ticker.to_owned(),
            None,
        )
        .await
    }

    /// `POST /portfolio/subaccounts`: create a new subaccount.
    pub async fn create_subaccount(
        &self,
        exchange_index: Option<u32>,
    ) -> Result<WriteOutcome<CreatedSubaccount>> {
        let request = Request::post("/portfolio/subaccounts")
            .body(CreateSubaccountBody { exchange_index })
            .authenticated();
        self.write(request, String::new(), None).await
    }

    /// `POST /portfolio/subaccounts/transfer`: move funds between the account's subaccounts.
    /// Idempotent via `SubaccountTransfer::client_transfer_id`.
    pub async fn transfer_between_subaccounts(
        &self,
        transfer: &SubaccountTransfer,
    ) -> Result<WriteOutcome<()>> {
        let request = Request::post("/portfolio/subaccounts/transfer")
            .body(transfer)
            .authenticated()
            .idempotent(true);
        self.write_empty(request, String::new(), None).await
    }

    /// `PUT /portfolio/subaccounts/netting`: enable or disable netting for one subaccount.
    /// Idempotent.
    pub async fn update_subaccount_netting(
        &self,
        subaccount_number: u32,
        enabled: bool,
    ) -> Result<WriteOutcome<()>> {
        let request = Request::put("/portfolio/subaccounts/netting")
            .body(UpdateSubaccountNettingBody {
                subaccount_number,
                enabled,
            })
            .authenticated();
        self.write_empty(request, String::new(), None).await
    }

    /// Sends a trading write and decodes the JSON response as `T`. Shared by the order and
    /// communications write surfaces.
    pub(crate) async fn write<T: DeserializeOwned>(
        &self,
        request: Request,
        ticker: String,
        client_order_id: Option<String>,
    ) -> Result<WriteOutcome<T>> {
        match self.dispatch(request, ticker, client_order_id).await? {
            WriteOutcome::Sent(body) => {
                let decoded = serde_json::from_str(&body)
                    .map_err(|source| KalshiError::Decode { body, source })?;
                Ok(WriteOutcome::Sent(decoded))
            }
            WriteOutcome::DryRun(dry) => Ok(WriteOutcome::DryRun(dry)),
        }
    }

    /// Like [`Self::write`] for endpoints that answer with no body: the raw body is discarded.
    pub(crate) async fn write_empty(
        &self,
        request: Request,
        ticker: String,
        client_order_id: Option<String>,
    ) -> Result<WriteOutcome<()>> {
        match self.dispatch(request, ticker, client_order_id).await? {
            WriteOutcome::Sent(_) => Ok(WriteOutcome::Sent(())),
            WriteOutcome::DryRun(dry) => Ok(WriteOutcome::DryRun(dry)),
        }
    }

    async fn dispatch(
        &self,
        request: Request,
        ticker: String,
        client_order_id: Option<String>,
    ) -> Result<WriteOutcome<String>> {
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
        let request = request.halted_when(|| HALTED.load(Ordering::SeqCst));
        let body = self.client.send_text(&request).await?;
        Ok(WriteOutcome::Sent(body))
    }

    /// Finds one order by client order id on `GET /portfolio/orders`, filtered by ticker.
    /// `None` when the live list holds no order with that id.
    async fn find_by_client_order_id(
        &self,
        ticker: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>> {
        let request = Request::get("/portfolio/orders")
            .query("ticker", ticker)
            .authenticated();
        let found: Vec<crate::types::Order> = self
            .client
            .paginate(request, "orders", 100, 1000)
            .try_collect()
            .await?;
        Ok(found
            .into_iter()
            .find(|found| found.client_order_id == client_order_id)
            .map(Self::confirmed))
    }

    /// Maps a read-side order (the lookup's result) onto the create-response shape.
    fn confirmed(found: crate::types::Order) -> Order {
        Order {
            order_id: found.order_id,
            client_order_id: Some(found.client_order_id),
            fill_count: found.fill_count_fp,
            remaining_count: found.remaining_count_fp,
            average_fill_price: None,
            average_fee_paid: None,
            ts_ms: found
                .last_update_time
                .or(found.created_time)
                .map_or(0, |updated| updated.timestamp_millis()),
        }
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
///
/// Build one with [`OrderBuilder`]; its required fields cannot be omitted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateOrderV2 {
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
    /// Idempotency key; makes the create safe to retry. [`OrderBuilder`] generates one by
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    /// Post-only: joins the book or is rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    /// Reduce-only: capped at the current position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    /// Expiration time, Unix milliseconds; the order is canceled at or after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_time: Option<i64>,
    /// Subaccount number, 0 for the primary account; omitted uses the key's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
    /// Order group this order joins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_group_id: Option<String>,
    /// Exchange shard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_index: Option<u32>,
}

/// Builder for [`CreateOrderV2`]. The fields Kalshi requires are constructor arguments and
/// cannot be omitted; the rest are optional setters. A random `client_order_id` is assigned at
/// [`Self::build`] unless [`Self::client_order_id`] or [`Self::without_client_order_id`] is
/// called, so a built order is safe to retry by default.
#[derive(Debug, Clone)]
pub struct OrderBuilder {
    ticker: String,
    side: OrderSide,
    count: Decimal,
    price: Decimal,
    time_in_force: TimeInForce,
    self_trade_prevention: SelfTradePrevention,
    client_order_id: Option<Option<String>>,
    post_only: Option<bool>,
    reduce_only: Option<bool>,
    expiration_time: Option<i64>,
    subaccount: Option<u32>,
    order_group_id: Option<String>,
    exchange_index: Option<u32>,
}

impl OrderBuilder {
    /// Starts a builder with the required fields: market ticker, side, contract count, limit
    /// price, time in force, and self-trade prevention mode.
    pub fn new(
        ticker: impl Into<String>,
        side: OrderSide,
        count: Decimal,
        price: Decimal,
        time_in_force: TimeInForce,
        self_trade_prevention: SelfTradePrevention,
    ) -> Self {
        Self {
            ticker: ticker.into(),
            side,
            count,
            price,
            time_in_force,
            self_trade_prevention,
            client_order_id: None,
            post_only: None,
            reduce_only: None,
            expiration_time: None,
            subaccount: None,
            order_group_id: None,
            exchange_index: None,
        }
    }

    /// Sets the caller's own idempotency key.
    pub fn client_order_id(mut self, client_order_id: impl Into<String>) -> Self {
        self.client_order_id = Some(Some(client_order_id.into()));
        self
    }

    /// Sends no `client_order_id`: the create is not safe to retry, and a failure after
    /// dispatch surfaces as [`KalshiError::Ambiguous`] instead of a plain error.
    pub fn without_client_order_id(mut self) -> Self {
        self.client_order_id = Some(None);
        self
    }

    /// Post-only: joins the book or is rejected.
    pub fn post_only(mut self, post_only: bool) -> Self {
        self.post_only = Some(post_only);
        self
    }

    /// Reduce-only: capped at the current position.
    pub fn reduce_only(mut self, reduce_only: bool) -> Self {
        self.reduce_only = Some(reduce_only);
        self
    }

    /// Expiration time, Unix milliseconds; the order is canceled at or after it.
    pub fn expiration_time(mut self, expiration_time: i64) -> Self {
        self.expiration_time = Some(expiration_time);
        self
    }

    /// Subaccount number, 0 for the primary account; omitted uses the key's default.
    pub fn subaccount(mut self, subaccount: u32) -> Self {
        self.subaccount = Some(subaccount);
        self
    }

    /// Order group this order joins.
    pub fn order_group_id(mut self, order_group_id: impl Into<String>) -> Self {
        self.order_group_id = Some(order_group_id.into());
        self
    }

    /// Exchange shard.
    pub fn exchange_index(mut self, exchange_index: u32) -> Self {
        self.exchange_index = Some(exchange_index);
        self
    }

    /// Builds the request, generating a random `client_order_id` when none was set.
    pub fn build(self) -> CreateOrderV2 {
        CreateOrderV2 {
            ticker: self.ticker,
            side: self.side,
            count: self.count,
            price: self.price,
            time_in_force: self.time_in_force,
            self_trade_prevention_type: self.self_trade_prevention,
            client_order_id: self
                .client_order_id
                .unwrap_or_else(|| Some(random_client_order_id())),
            post_only: self.post_only,
            reduce_only: self.reduce_only,
            expiration_time: self.expiration_time,
            subaccount: self.subaccount,
            order_group_id: self.order_group_id,
            exchange_index: self.exchange_index,
        }
    }
}

/// Token cost of a batch: the sum of its per-item costs, saturating on overflow.
pub(crate) fn batch_cost(items: usize, per_item: u32) -> u32 {
    u32::try_from(items)
        .unwrap_or(u32::MAX)
        .saturating_mul(per_item)
}

/// A version-4 UUID formatted as a client order id.
fn random_client_order_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
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

/// Fields for `POST /portfolio/events/orders/{order_id}/amend`. All four required fields are
/// struct fields, so none can be omitted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AmendOrder {
    /// Market ticker of the order.
    pub ticker: String,
    /// Side of the order.
    pub side: OrderSide,
    /// Updated price in dollars, sent as a fixed-point string.
    #[serde(serialize_with = "decimal_as_string")]
    pub price: Decimal,
    /// Updated total (max fillable) count: already-filled count plus the desired resting
    /// remaining count, sent as a fixed-point string.
    #[serde(serialize_with = "decimal_as_string")]
    pub count: Decimal,
    /// The original client order id, when the order was created with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    /// New client order id after the amend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_client_order_id: Option<String>,
    /// New expiration time in Unix seconds; 0 removes it (good-till-canceled). Omit to keep
    /// the current expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_time: Option<i64>,
}

/// Response of `POST /portfolio/events/orders/{order_id}/amend`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AmendedOrder {
    /// Kalshi order id.
    pub order_id: String,
    /// The client order id after the amend, when present.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Resting contracts after the amend; present when the resting size changed.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub remaining_count: Option<Decimal>,
    /// Contracts filled by the amend; present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub fill_count: Option<Decimal>,
    /// Volume-weighted average fill price of the amend's fills, present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fill_price: Option<Decimal>,
    /// Volume-weighted average fee per contract of the amend's fills, present when fills
    /// occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fee_paid: Option<Decimal>,
    /// Engine timestamp, Unix milliseconds.
    pub ts_ms: i64,
}

/// The size change of `POST /portfolio/events/orders/{order_id}/decrease`: exactly one of the
/// two variants, so a request with both fields or with neither does not exist.
///
/// ```rust,compile_fail
/// use furball::DecreaseAmount;
/// // Both fields at once does not compile: the variants take exactly one count.
/// let _ = DecreaseAmount::By(1.into(), 2.into());
/// ```
///
/// ```rust,compile_fail
/// use furball::DecreaseAmount;
/// // Neither field does not compile: there is no empty variant.
/// let _ = DecreaseAmount::Neither;
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum DecreaseAmount {
    /// Reduce the order's remaining count by this many contracts.
    By(Decimal),
    /// Reduce the order's remaining count to this many contracts.
    To(Decimal),
}

impl Serialize for DecreaseAmount {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::By(reduce_by) => {
                #[derive(Serialize)]
                struct Body {
                    #[serde(serialize_with = "decimal_as_string")]
                    reduce_by: Decimal,
                }
                Body {
                    reduce_by: *reduce_by,
                }
                .serialize(serializer)
            }
            Self::To(reduce_to) => {
                #[derive(Serialize)]
                struct Body {
                    #[serde(serialize_with = "decimal_as_string")]
                    reduce_to: Decimal,
                }
                Body {
                    reduce_to: *reduce_to,
                }
                .serialize(serializer)
            }
        }
    }
}

/// Response of `POST /portfolio/events/orders/{order_id}/decrease`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DecreasedOrder {
    /// Kalshi order id.
    pub order_id: String,
    /// The client order id, when present.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Contracts remaining after the decrease.
    #[serde(deserialize_with = "decimal_from_string")]
    pub remaining_count: Decimal,
    /// Engine timestamp, Unix milliseconds.
    pub ts_ms: i64,
}

#[derive(Serialize)]
struct BatchCreateBody {
    orders: Vec<CreateOrderV2>,
}

#[derive(Deserialize)]
struct BatchCreateResponse {
    orders: Vec<BatchCreateResult>,
}

#[derive(Serialize)]
struct BatchCancelBody {
    orders: Vec<BatchCancelOrder>,
}

#[derive(Deserialize)]
struct BatchCancelResponse {
    orders: Vec<BatchCancelResult>,
}

/// One entry of a `DELETE /portfolio/events/orders/batched` request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchCancelOrder {
    /// Kalshi order id to cancel.
    pub order_id: String,
    /// Subaccount the order is on, 0 for the primary account; omitted uses the key's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
    /// Exchange shard; omitted auto-routes when `market_ticker` is set, otherwise defaults to 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_index: Option<u32>,
    /// Market ticker; required for auto-routing when `exchange_index` is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_ticker: Option<String>,
}

/// One result of `POST /portfolio/events/orders/batched`: exactly one per requested item, in
/// request order. A failed item carries [`Self::error`] and no order fields; a successful item
/// carries the order fields and no error.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BatchCreateResult {
    /// Kalshi order id; present when the item was accepted.
    #[serde(default)]
    pub order_id: Option<String>,
    /// The client order id, when the item had one.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Contracts filled immediately; present when the item was accepted.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub fill_count: Option<Decimal>,
    /// Contracts remaining after placement; present when the item was accepted.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub remaining_count: Option<Decimal>,
    /// Volume-weighted average fill price; present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fill_price: Option<Decimal>,
    /// Volume-weighted average fee per contract; present when fills occurred.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub average_fee_paid: Option<Decimal>,
    /// Engine timestamp, Unix milliseconds; absent when the item errored.
    #[serde(default)]
    pub ts_ms: Option<i64>,
    /// Kalshi's error for this item; present when the item failed.
    #[serde(default)]
    pub error: Option<BatchItemError>,
}

/// One result of `DELETE /portfolio/events/orders/batched`: exactly one per requested item, in
/// request order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BatchCancelResult {
    /// Kalshi order id.
    pub order_id: String,
    /// The client order id, when present.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Contracts the cancel removed; zero when the item errored.
    #[serde(deserialize_with = "decimal_from_string")]
    pub reduced_by: Decimal,
    /// Engine timestamp, Unix milliseconds; absent when the item errored.
    #[serde(default)]
    pub ts_ms: Option<i64>,
    /// Kalshi's error for this item; present when the item failed.
    #[serde(default)]
    pub error: Option<BatchItemError>,
}

/// Kalshi's per-item error in a batch order response.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BatchItemError {
    /// Kalshi's numeric error code; numeric strings are accepted as well.
    #[serde(deserialize_with = "item_error_code")]
    pub code: i64,
    /// Kalshi's error message.
    pub message: String,
}

/// Decodes a batch item error code: a JSON number, or a string that parses as one.
fn item_error_code<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<i64, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("error code is not an integer")),
        serde_json::Value::String(text) => text
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("error code is not a number: {text}"))),
        other => Err(serde::de::Error::custom(format!(
            "error code is not a number: {other:?}"
        ))),
    }
}

/// Fields for `POST /portfolio/order_groups/create`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewOrderGroup {
    /// Maximum number of contracts that can be matched within the group over a rolling
    /// 15-second window, sent as a fixed-point string.
    #[serde(serialize_with = "decimal_as_string")]
    pub contracts_limit_fp: Decimal,
    /// Subaccount that owns the group, 0 for the primary account; omitted uses the key's
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
    /// Exchange shard; defaults to 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_index: Option<u32>,
}

#[derive(Serialize)]
struct UpdateOrderGroupLimitBody {
    #[serde(serialize_with = "decimal_as_string")]
    contracts_limit_fp: Decimal,
}

/// An order group, as returned by `POST /portfolio/order_groups/create`.
///
/// Attach orders to a group with [`OrderBuilder::order_group_id`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderGroup {
    /// Unique identifier of the order group.
    pub order_group_id: String,
    /// Subaccount that owns the group, 0 for the primary account.
    pub subaccount: u32,
    /// Exchange shard, when present.
    #[serde(default)]
    pub exchange_index: Option<u32>,
}

fn optional_decimal_as_string<S: serde::Serializer>(
    value: &Option<Decimal>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(value) => decimal_as_string(value, serializer),
        None => serializer.serialize_none(),
    }
}

/// Unit of an exit trigger's slippage allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlippageUnit {
    /// Basis points of the trigger price.
    #[serde(rename = "bps")]
    Bps,
    /// Cents of a dollar.
    #[serde(rename = "cents")]
    Cents,
}

/// Slippage allowance for an exit trigger: how far past the trigger price the resulting exit
/// order may trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitSlippage {
    /// Unit of [`Self::value`].
    pub unit: SlippageUnit,
    /// Slippage amount in [`Self::unit`] units.
    pub value: i64,
}

/// Kind of an exit trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ExitTriggerKind {
    /// A paired stop-loss and take-profit.
    Bracket,
    /// A kind this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for ExitTriggerKind {
    fn from(value: String) -> Self {
        match value.as_str() {
            "bracket" => Self::Bracket,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl From<ExitTriggerKind> for String {
    fn from(value: ExitTriggerKind) -> Self {
        match value {
            ExitTriggerKind::Bracket => "bracket".to_owned(),
            ExitTriggerKind::Other(other) => other,
        }
    }
}

/// Stop-loss and/or take-profit prices for an exit trigger. Prices are YES prices in dollars.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExitTrigger {
    /// Slippage allowance for the exit orders; omitted uses the exchange default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slippage: Option<ExitSlippage>,
    /// Trigger kind; currently only [`ExitTriggerKind::Bracket`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ExitTriggerKind>,
    /// Stop-loss price; the exit fires when the price crosses it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_decimal_as_string",
        deserialize_with = "optional_decimal_from_string"
    )]
    pub stop_loss_price: Option<Decimal>,
    /// Take-profit price; the exit fires when the price crosses it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_decimal_as_string",
        deserialize_with = "optional_decimal_from_string"
    )]
    pub take_profit_price: Option<Decimal>,
}

#[derive(Serialize)]
struct CreateOrderWithExitTriggerBody<'a> {
    #[serde(flatten)]
    order: &'a CreateOrderV2,
    exit_trigger: &'a ExitTrigger,
}

/// Status of an event contract exit trigger.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum ExitTriggerStatus {
    /// Waiting for the entry order to fill before arming.
    PendingOnEntry,
    /// Armed and waiting to trigger.
    Active,
    /// Triggered; the exit order filled.
    Filled,
    /// Canceled before triggering.
    Canceled,
    /// The trigger failed.
    Failed,
    /// A status this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for ExitTriggerStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "pending_on_entry" => Self::PendingOnEntry,
            "active" => Self::Active,
            "filled" => Self::Filled,
            "canceled" => Self::Canceled,
            "failed" => Self::Failed,
            _ => Self::Other(value),
        }
    }
}

/// Which leg of a bracket trigger fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggeredLeg {
    /// The stop-loss leg.
    StopLoss,
    /// The take-profit leg.
    TakeProfit,
}

/// An event contract exit trigger, as returned by
/// `PUT /portfolio/positions/{ticker}/exit_trigger`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventContractExitTrigger {
    /// Slippage allowance, when set.
    #[serde(default)]
    pub slippage: Option<ExitSlippage>,
    /// Trigger identifier.
    pub id: String,
    /// Market ticker of the guarded position.
    pub ticker: String,
    /// Outcome side of the guarded position.
    pub side: crate::types::OutcomeSide,
    /// Subaccount number of the guarded position, 0 for the primary account.
    pub subaccount_number: u32,
    /// Trigger kind.
    pub kind: ExitTriggerKind,
    /// Stop-loss price, when set.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub stop_loss_price: Option<Decimal>,
    /// Take-profit price, when set.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub take_profit_price: Option<Decimal>,
    /// Trigger status.
    pub status: ExitTriggerStatus,
    /// Why the trigger is in its current state, when present.
    #[serde(default)]
    pub status_reason: Option<String>,
    /// The order that fired the trigger, when it triggered.
    #[serde(default)]
    pub triggered_order_id: Option<String>,
    /// The leg that fired, when it triggered.
    #[serde(default)]
    pub triggered_leg: Option<TriggeredLeg>,
    /// When the trigger was created.
    pub created_time: DateTime<Utc>,
    /// When the trigger was last updated.
    pub updated_time: DateTime<Utc>,
}

#[derive(Serialize)]
struct CreateSubaccountBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    exchange_index: Option<u32>,
}

/// The newly created subaccount, as returned by `POST /portfolio/subaccounts`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreatedSubaccount {
    /// Sequential number assigned to the subaccount (1-63).
    pub subaccount_number: u32,
}

/// Fields for `POST /portfolio/subaccounts/transfer`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubaccountTransfer {
    /// Client-provided UUID; makes the transfer idempotent.
    pub client_transfer_id: String,
    /// Source subaccount number, 0 for the primary account.
    pub from_subaccount: u32,
    /// Destination subaccount number, 0 for the primary account.
    pub to_subaccount: u32,
    /// Amount to transfer in cents.
    pub amount_cents: i64,
    /// Exchange shard; defaults to 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_index: Option<u32>,
}

#[derive(Serialize)]
struct UpdateSubaccountNettingBody {
    subaccount_number: u32,
    enabled: bool,
}
