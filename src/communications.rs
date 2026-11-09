//! RFQ (request for quote) and quote endpoints under `/communications`, for negotiated large
//! trades.
//!
//! The read side ([`Client::get_rfq`], [`Client::list_rfqs`], [`Client::get_quote`] and
//! [`Client::list_quotes`]) is available in every build and is authenticated like the portfolio
//! reads. The write side (create, delete, accept and confirm on `TradingClient`) is behind the
//! `trading` cargo feature and carries the same safety gates as the order writes: demo
//! environment by default, the global halt switch, and dry-run. All writes are charged to the
//! Write bucket: the API serves the communications endpoints from exchange shard 1, which this
//! crate models as the single Write bucket.
//!
//! The quote lifecycle is a two-step lock:
//!
//! ```text
//! open -> accepted -> confirmed -> executed
//!   |          |
//!   +----------+-> cancelled (a creator withdraws, or the confirmation window closes)
//! ```
//!
//! The RFQ creator accepts a quote in the [`QuoteStatus::Open`] state, which locks it as
//! [`QuoteStatus::Accepted`] and starts the confirmation window; the quote creator then
//! confirms before the window closes. Once confirmed, neither party can withdraw. The quote
//! that fills can be another maker's quote at the same price.

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::client::{Client, Request};
use crate::error::Result;
use crate::types::{decimal_from_string, optional_decimal_from_string};

#[cfg(feature = "trading")]
use crate::rate_limit::DEFAULT_COST;
#[cfg(feature = "trading")]
use crate::trading::{TradingClient, WriteOutcome, batch_cost};
#[cfg(feature = "trading")]
use crate::types::OutcomeSide;
#[cfg(feature = "trading")]
use crate::types::decimal_as_string;

/// State of an RFQ.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum RfqStatus {
    /// Open: the RFQ accepts quotes.
    Open,
    /// Closed: the RFQ was deleted, expired, or already executed.
    Closed,
    /// A status this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for RfqStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "open" => Self::Open,
            "closed" => Self::Closed,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl RfqStatus {
    /// The wire value Kalshi's `status` filter expects.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Other(value) => value,
        }
    }
}

/// State of a quote.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum QuoteStatus {
    /// Open: the RFQ creator can accept it and its creator can delete it.
    Open,
    /// Accepted by the RFQ creator; the quote creator must confirm before the window closes.
    Accepted,
    /// Confirmed by the quote creator; the execution timer is running.
    Confirmed,
    /// Executed: both parties locked in and the trade ran.
    Executed,
    /// Cancelled: a creator withdrew it, or the confirmation window closed.
    Cancelled,
    /// A status this crate does not know yet, kept verbatim.
    Other(String),
}

impl From<String> for QuoteStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "open" => Self::Open,
            "accepted" => Self::Accepted,
            "confirmed" => Self::Confirmed,
            "executed" => Self::Executed,
            "cancelled" => Self::Cancelled,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl QuoteStatus {
    /// Whether a quote in this state can be accepted by the RFQ creator.
    pub fn can_accept(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// Whether a quote in this state can be confirmed by the quote creator.
    pub fn can_confirm(&self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// Whether a quote in this state can be deleted by its creator.
    pub fn can_delete(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// The wire value Kalshi's `status` filter expects.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Confirmed => "confirmed",
            Self::Executed => "executed",
            Self::Cancelled => "cancelled",
            Self::Other(value) => value,
        }
    }
}

/// A request for quote, as returned by the RFQ endpoints.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Rfq {
    /// UUID of the RFQ; pass it unchanged to the RFQ and quote endpoints.
    #[serde(rename = "id")]
    pub rfq_id: String,
    /// Public communications id of the RFQ creator (pseudonymous).
    pub creator_id: String,
    /// Market ticker.
    pub market_ticker: String,
    /// Contracts requested, in 0.01-contract increments.
    #[serde(deserialize_with = "decimal_from_string")]
    pub contracts_fp: Decimal,
    /// Target cost in dollars, when the RFQ was sized by cost.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub target_cost_dollars: Option<Decimal>,
    /// True when the target cost is principal-only and taker fees are charged on top.
    #[serde(default)]
    pub target_cost_excludes_fees: bool,
    /// RFQ state.
    pub status: RfqStatus,
    /// When the RFQ was created.
    pub created_ts: DateTime<Utc>,
    /// When the RFQ was last updated.
    #[serde(default)]
    pub updated_ts: Option<DateTime<Utc>>,
    /// Whether the remainder of the RFQ rests after execution.
    #[serde(default)]
    pub rest_remainder: bool,
    /// Reason for cancellation, when the RFQ was cancelled.
    #[serde(default)]
    pub cancellation_reason: Option<String>,
    /// Subaccount of the RFQ creator, visible to the creator.
    #[serde(default)]
    pub creator_subaccount: Option<u32>,
    /// User id of the RFQ creator, visible to the creator.
    #[serde(default)]
    pub creator_user_id: Option<String>,
    /// When the RFQ was cancelled.
    #[serde(default)]
    pub cancelled_ts: Option<DateTime<Utc>>,
    /// Identifier of the RFQ batch this RFQ is a child of, when present.
    #[serde(default)]
    pub rfq_batch_id: Option<String>,
    /// Ticker of the multivariate event collection this market belongs to, when present.
    #[serde(default)]
    pub mve_collection_ticker: Option<String>,
    /// Selected legs of the multivariate event collection, when present.
    #[serde(default)]
    pub mve_selected_legs: Vec<MveSelectedLeg>,
}

/// One selected leg of a multivariate event collection on an RFQ.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MveSelectedLeg {
    /// Event ticker of the leg.
    pub event_ticker: String,
    /// Market ticker of the leg.
    pub market_ticker: String,
    /// Side of the selected market.
    pub side: String,
    /// Settlement value of the YES side in dollars, filled after determination.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub yes_settlement_value_dollars: Option<Decimal>,
}

/// A quote on an RFQ, as returned by the quote endpoints.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Quote {
    /// UUID of the quote; pass it unchanged to the quote endpoints.
    #[serde(rename = "id")]
    pub quote_id: String,
    /// UUID of the RFQ this quote responds to.
    pub rfq_id: String,
    /// Public communications id of the quote creator.
    pub creator_id: String,
    /// Public communications id of the RFQ creator (pseudonymous).
    pub rfq_creator_id: String,
    /// Market ticker.
    pub market_ticker: String,
    /// Contracts in the quote, in 0.01-contract increments.
    #[serde(deserialize_with = "decimal_from_string")]
    pub contracts_fp: Decimal,
    /// Bid price for YES contracts, in dollars.
    #[serde(deserialize_with = "decimal_from_string")]
    pub yes_bid_dollars: Decimal,
    /// Bid price for NO contracts, in dollars.
    #[serde(deserialize_with = "decimal_from_string")]
    pub no_bid_dollars: Decimal,
    /// When the quote was created.
    pub created_ts: DateTime<Utc>,
    /// When the quote was last updated.
    pub updated_ts: DateTime<Utc>,
    /// Quote state.
    pub status: QuoteStatus,
    /// The side the RFQ creator accepted, when the quote was accepted.
    #[serde(default)]
    pub accepted_side: Option<AcceptedSide>,
    /// When the quote was accepted.
    #[serde(default)]
    pub accepted_ts: Option<DateTime<Utc>>,
    /// When the quote was confirmed.
    #[serde(default)]
    pub confirmed_ts: Option<DateTime<Utc>>,
    /// When the quote was executed.
    #[serde(default)]
    pub executed_ts: Option<DateTime<Utc>>,
    /// When the quote was cancelled.
    #[serde(default)]
    pub cancelled_ts: Option<DateTime<Utc>>,
    /// Whether the remainder of the quote rests after execution.
    #[serde(default)]
    pub rest_remainder: bool,
    /// Whether the quote creator's order is post-only, visible to the quote creator.
    #[serde(default)]
    pub post_only: bool,
    /// Reason for cancellation, when the quote was cancelled.
    #[serde(default)]
    pub cancellation_reason: Option<String>,
    /// User id of the quote creator, visible to the quote creator.
    #[serde(default)]
    pub creator_user_id: Option<String>,
    /// User id of the RFQ creator, visible to the RFQ creator.
    #[serde(default)]
    pub rfq_creator_user_id: Option<String>,
    /// Order id for the RFQ creator, visible to the RFQ creator.
    #[serde(default)]
    pub rfq_creator_order_id: Option<String>,
    /// Order id for the quote creator, visible to the quote creator.
    #[serde(default)]
    pub creator_order_id: Option<String>,
    /// Subaccount of the quote creator, visible to the quote creator.
    #[serde(default)]
    pub creator_subaccount: Option<u32>,
    /// Subaccount of the RFQ creator, visible to the RFQ creator.
    #[serde(default)]
    pub rfq_creator_subaccount: Option<u32>,
    /// Total value requested in the RFQ, in dollars, when the RFQ was sized by cost.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub rfq_target_cost_dollars: Option<Decimal>,
    /// True when the RFQ's target cost is principal-only.
    #[serde(default)]
    pub target_cost_excludes_fees: bool,
    /// YES contracts offered in the quote, when the RFQ was sized by cost.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub yes_contracts_fp: Option<Decimal>,
    /// NO contracts offered in the quote, when the RFQ was sized by cost.
    #[serde(default, deserialize_with = "optional_decimal_from_string")]
    pub no_contracts_fp: Option<Decimal>,
}

/// Filters for `GET /communications/rfqs`.
#[derive(Debug, Clone, Default)]
pub struct RfqFilter {
    /// Market ticker.
    pub market_ticker: Option<String>,
    /// Event ticker; only a single event is supported.
    pub event_ticker: Option<String>,
    /// RFQ state.
    pub status: Option<RfqStatus>,
    /// Subaccount number, 0 for the primary account; omitted returns all subaccounts.
    pub subaccount: Option<u32>,
    /// Only RFQs created by the authenticated user.
    pub created_by_me: bool,
}

impl RfqFilter {
    fn apply(&self) -> Request {
        Request::get("/communications/rfqs")
            .query_opt("market_ticker", self.market_ticker.as_deref())
            .query_opt("event_ticker", self.event_ticker.as_deref())
            .query_opt("status", self.status.as_ref().map(RfqStatus::as_str))
            .query_opt("subaccount", self.subaccount)
            .query_opt("user_filter", self.created_by_me.then_some("self"))
            .authenticated()
    }
}

/// Filters for `GET /communications/quotes`.
#[derive(Debug, Clone, Default)]
pub struct QuoteFilter {
    /// RFQ the quotes respond to.
    pub rfq_id: Option<String>,
    /// Quote state.
    pub status: Option<QuoteStatus>,
    /// Quotes last updated after this Unix timestamp.
    pub min_ts: Option<i64>,
    /// Quotes last updated before this Unix timestamp.
    pub max_ts: Option<i64>,
    /// Only quotes created by the authenticated user.
    pub created_by_me: bool,
    /// Only quotes on RFQs created by the authenticated user.
    pub on_own_rfq: bool,
    /// RFQ creator subtrader id (FCM members only).
    pub rfq_creator_subtrader_id: Option<String>,
}

impl QuoteFilter {
    fn apply(&self) -> Request {
        Request::get("/communications/quotes")
            .query_opt("rfq_id", self.rfq_id.as_deref())
            .query_opt("status", self.status.as_ref().map(QuoteStatus::as_str))
            .query_opt("min_ts", self.min_ts)
            .query_opt("max_ts", self.max_ts)
            .query_opt("user_filter", self.created_by_me.then_some("self"))
            .query_opt("rfq_user_filter", self.on_own_rfq.then_some("self"))
            .query_opt(
                "rfq_creator_subtrader_id",
                self.rfq_creator_subtrader_id.as_deref(),
            )
            .authenticated()
    }
}

#[derive(Deserialize)]
struct RfqResponse {
    rfq: Rfq,
}

#[derive(Deserialize)]
struct QuoteResponse {
    quote: Quote,
}

impl Client {
    /// `GET /communications/rfqs/{rfq_id}`: one RFQ. Needs credentials.
    pub async fn get_rfq(&self, rfq_id: &str) -> Result<Rfq> {
        let request = Request::get(format!("/communications/rfqs/{rfq_id}")).authenticated();
        let response: RfqResponse = self.send(&request).await?;
        Ok(response.rfq)
    }

    /// `GET /communications/rfqs`: RFQs matching `filter`, following cursors until the list is
    /// exhausted or at most `max_items` RFQs have been collected. Needs credentials.
    pub async fn list_rfqs(
        &self,
        filter: &RfqFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Rfq>> {
        self.paginate(filter.apply(), "rfqs", limit, max_items)
            .try_collect()
            .await
    }

    /// `GET /communications/rfqs/{rfq_id}/quotes/{quote_id}`: one quote scoped to its RFQ.
    /// Needs credentials.
    pub async fn get_quote(&self, rfq_id: &str, quote_id: &str) -> Result<Quote> {
        let request = Request::get(format!("/communications/rfqs/{rfq_id}/quotes/{quote_id}"))
            .authenticated();
        let response: QuoteResponse = self.send(&request).await?;
        Ok(response.quote)
    }

    /// `GET /communications/quotes`: quotes matching `filter`, following cursors until the list
    /// is exhausted or at most `max_items` quotes have been collected. Needs credentials.
    pub async fn list_quotes(
        &self,
        filter: &QuoteFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Quote>> {
        self.paginate(filter.apply(), "quotes", limit, max_items)
            .try_collect()
            .await
    }
}

/// The side of a quote the RFQ creator accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcceptedSide {
    /// The YES bid.
    #[serde(rename = "yes")]
    Yes,
    /// The NO bid.
    #[serde(rename = "no")]
    No,
}

/// Size of a new RFQ: a contract count or a target cost, exactly one of the two.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub enum RfqSize {
    /// Request this many contracts, in 0.01-contract increments.
    Contracts(Decimal),
    /// Request a target cost in dollars; the exchange derives the contract count from it.
    TargetCost {
        /// Target cost in dollars.
        dollars: Decimal,
        /// Size quotes against the target cost as principal only, with taker fees charged on
        /// top.
        excludes_fees: bool,
    },
}

/// A new RFQ for `POST /communications/rfqs`.
///
/// Exactly one size mode is sent: [`RfqSize::Contracts`] serializes `contracts_fp` and
/// [`RfqSize::TargetCost`] serializes `target_cost_dollars` (plus the fee flag), never both.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub struct NewRfq {
    /// Market ticker.
    pub market_ticker: String,
    /// Size of the RFQ.
    pub size: RfqSize,
    /// Whether to rest the remainder of the RFQ after execution.
    pub rest_remainder: bool,
    /// Delete the creator's existing open RFQs when this one is created.
    pub replace_existing: bool,
    /// Hide the creator's communications id from other users until execution.
    pub obscure_creator_id: bool,
    /// Subtrader to create the RFQ for (FCM members only).
    pub subtrader_id: Option<String>,
    /// Subaccount to create the RFQ for, 0 for the primary account.
    pub subaccount: Option<u32>,
}

#[cfg(feature = "trading")]
impl Serialize for NewRfq {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("NewRfq", 7)?;
        state.serialize_field("market_ticker", &self.market_ticker)?;
        match &self.size {
            RfqSize::Contracts(contracts) => {
                state.serialize_field("contracts_fp", &contracts.to_string())?;
            }
            RfqSize::TargetCost {
                dollars,
                excludes_fees,
            } => {
                state.serialize_field("target_cost_dollars", &dollars.to_string())?;
                state.serialize_field("target_cost_excludes_fees", excludes_fees)?;
            }
        }
        state.serialize_field("rest_remainder", &self.rest_remainder)?;
        if self.replace_existing {
            state.serialize_field("replace_existing", &true)?;
        }
        if self.obscure_creator_id {
            state.serialize_field("obscure_creator_id", &true)?;
        }
        if let Some(subtrader_id) = &self.subtrader_id {
            state.serialize_field("subtrader_id", subtrader_id)?;
        }
        if let Some(subaccount) = self.subaccount {
            state.serialize_field("subaccount", &subaccount)?;
        }
        state.end()
    }
}

/// The id of a newly created RFQ, as returned by `POST /communications/rfqs`.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreatedRfq {
    /// UUID of the created RFQ; pass it unchanged to the RFQ and quote endpoints.
    #[serde(rename = "id")]
    pub rfq_id: String,
}

/// A quote to create for an RFQ, for `POST /communications/quotes`.
///
/// A quote bids on both sides of the market for the full RFQ size; the RFQ creator later
/// picks one side to accept.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, Serialize)]
pub struct NewQuote {
    /// UUID of the RFQ to quote on.
    pub rfq_id: String,
    /// Bid price for YES contracts, in dollars.
    #[serde(serialize_with = "decimal_as_string")]
    pub yes_bid: Decimal,
    /// Bid price for NO contracts, in dollars.
    #[serde(serialize_with = "decimal_as_string")]
    pub no_bid: Decimal,
    /// Whether to rest the remainder of the quote after execution.
    pub rest_remainder: bool,
    /// The quote creator's resting order is cancelled rather than crossed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_only: bool,
    /// Subaccount to place the quote under, 0 for the primary account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
}

/// The id of a newly created quote, as returned by `POST /communications/quotes`.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreatedQuote {
    /// UUID of the created quote; pass it unchanged to the quote endpoints.
    #[serde(rename = "id")]
    pub quote_id: String,
}

#[cfg(feature = "trading")]
#[derive(Serialize)]
struct AcceptQuoteBody {
    accepted_side: AcceptedSide,
}

#[cfg(feature = "trading")]
impl TradingClient {
    /// `POST /communications/rfqs`: create an RFQ. Up to 100 open RFQs at a time.
    ///
    /// The request carries no caller-chosen id, so it is not marked idempotent: a timeout,
    /// connection failure, or HTTP 5xx surfaces as [`crate::KalshiError::Ambiguous`] because
    /// the RFQ may have been created. Reconcile with `Client::list_rfqs` before retrying.
    /// Charged to the Write bucket (the API serves communications endpoints from exchange
    /// shard 1, which this crate models as the single Write bucket).
    pub async fn create_rfq(&self, rfq: &NewRfq) -> Result<WriteOutcome<CreatedRfq>> {
        let request = Request::post("/communications/rfqs")
            .body(rfq)
            .authenticated();
        self.write(request, rfq.market_ticker.clone(), None).await
    }

    /// `DELETE /communications/rfqs/{rfq_id}`: delete an RFQ so it can no longer be quoted.
    /// Idempotent.
    pub async fn delete_rfq(&self, rfq_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!("/communications/rfqs/{rfq_id}")).authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `POST /communications/quotes`: create a quote in response to an RFQ.
    ///
    /// The request carries no caller-chosen id, so it is not marked idempotent: a timeout,
    /// connection failure, or HTTP 5xx surfaces as [`crate::KalshiError::Ambiguous`] because
    /// the quote may have been created. Reconcile with `Client::list_quotes` before retrying.
    pub async fn create_quote(&self, quote: &NewQuote) -> Result<WriteOutcome<CreatedQuote>> {
        let request = Request::post("/communications/quotes")
            .body(quote)
            .authenticated();
        self.write(request, String::new(), None).await
    }

    /// `DELETE /communications/rfqs/{rfq_id}/quotes/{quote_id}`: delete a quote so it can no
    /// longer be accepted.
    ///
    /// Only the quote's creator can delete it, and only while it is in the [`QuoteStatus::Open`]
    /// state (`quote.status.can_delete()` checks that locally); anything else is rejected by
    /// the API. Idempotent.
    pub async fn delete_quote(&self, rfq_id: &str, quote_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!("/communications/rfqs/{rfq_id}/quotes/{quote_id}"))
                .authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /communications/rfqs/{rfq_id}/quotes/{quote_id}/accept`: accept a quote on behalf
    /// of the RFQ creator.
    ///
    /// The quote must exist (a missing or already deleted quote is HTTP 404) and be in the
    /// [`QuoteStatus::Open`] state: `quote.status.can_accept()` checks that locally. Accept
    /// locks the quote as [`QuoteStatus::Accepted`] and starts the confirmation window; the
    /// quote creator must then call [`Self::confirm_rfq_quote`] before the window closes. The
    /// quote that fills can be another maker's quote at the same price. Idempotent by method.
    pub async fn accept_rfq_quote(
        &self,
        rfq_id: &str,
        quote_id: &str,
        side: AcceptedSide,
    ) -> Result<WriteOutcome<()>> {
        let request = Request::put(format!(
            "/communications/rfqs/{rfq_id}/quotes/{quote_id}/accept"
        ))
        .body(AcceptQuoteBody {
            accepted_side: side,
        })
        .authenticated();
        self.write_empty(request, String::new(), None).await
    }

    /// `PUT /communications/quotes/{quote_id}/accept`: accept a quote.
    ///
    /// The API deprecates this endpoint in favour of [`Self::accept_rfq_quote`], which new
    /// code should call. The quote must exist and be in the [`QuoteStatus::Open`] state.
    /// Idempotent by method.
    pub async fn accept_quote(
        &self,
        quote_id: &str,
        side: AcceptedSide,
    ) -> Result<WriteOutcome<()>> {
        let request = Request::put(format!("/communications/quotes/{quote_id}/accept"))
            .body(AcceptQuoteBody {
                accepted_side: side,
            })
            .authenticated();
        self.write_empty(request, String::new(), None).await
    }

    /// `PUT /communications/rfqs/{rfq_id}/quotes/{quote_id}/confirm`: confirm a quote on
    /// behalf of the quote creator, starting the execution timer.
    ///
    /// The quote must exist (HTTP 404 otherwise) and be in the [`QuoteStatus::Accepted`]
    /// state: `quote.status.can_confirm()` checks that locally. Once confirmed, neither party
    /// can withdraw. Idempotent by method.
    pub async fn confirm_rfq_quote(
        &self,
        rfq_id: &str,
        quote_id: &str,
    ) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::put(format!(
                "/communications/rfqs/{rfq_id}/quotes/{quote_id}/confirm"
            ))
            .authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /communications/quotes/{quote_id}/confirm`: confirm a quote.
    ///
    /// The API deprecates this endpoint in favour of [`Self::confirm_rfq_quote`]. The quote
    /// must exist and be in the [`QuoteStatus::Accepted`] state. Idempotent by method.
    pub async fn confirm_quote(&self, quote_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::put(format!("/communications/quotes/{quote_id}/confirm")).authenticated(),
            String::new(),
            None,
        )
        .await
    }
}

/// State of an RFQ batch.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum RfqBatchStatus {
    /// Open: the batch takes batch quotes.
    Open,
    /// Accepted: the batch is waiting for its winning makers to confirm, or is executing.
    Accepted,
    /// Closed: the batch is finished.
    Closed,
    /// A status this crate does not know yet, kept verbatim.
    Other(String),
}

#[cfg(feature = "trading")]
impl From<String> for RfqBatchStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "open" => Self::Open,
            "accepted" => Self::Accepted,
            "closed" => Self::Closed,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// An RFQ batch with its child RFQs, as returned by the batch endpoints.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RfqBatch {
    /// UUID of the RFQ batch; pass it unchanged to the batch endpoints.
    #[serde(rename = "id")]
    pub rfq_batch_id: String,
    /// Public communications id of the batch creator (pseudonymous).
    pub creator_id: String,
    /// State of the batch.
    pub status: RfqBatchStatus,
    /// When the batch was created.
    pub created_ts: DateTime<Utc>,
    /// When a batch quote on the batch was accepted.
    #[serde(default)]
    pub accepted_ts: Option<DateTime<Utc>>,
    /// When the batch closed.
    #[serde(default)]
    pub closed_ts: Option<DateTime<Utc>>,
    /// The child RFQs, in the order they were requested.
    pub rfqs: Vec<Rfq>,
    /// When the batch was last updated, visible to the batch creator.
    #[serde(default)]
    pub updated_ts: Option<DateTime<Utc>>,
    /// Why the batch closed, visible to the batch creator.
    #[serde(default)]
    pub close_reason: Option<String>,
    /// The `client_batch_id` given at creation, visible to the batch creator.
    #[serde(default)]
    pub client_batch_id: Option<String>,
    /// User id of the batch creator, visible to the creator.
    #[serde(default)]
    pub creator_user_id: Option<String>,
    /// Subaccount of the batch creator, visible to the creator.
    #[serde(default)]
    pub creator_subaccount: Option<u32>,
}

/// One leg of a combo: a market ticker, its event ticker, and the side.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TickerPair {
    /// Market ticker identifier.
    pub market_ticker: String,
    /// Event ticker identifier.
    pub event_ticker: String,
    /// Side of the market (yes or no).
    pub side: OutcomeSide,
}

/// Size of one child RFQ of a batch, in place of the batch's size: a contract count or a target
/// cost, exactly one of the two.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub enum RfqChildSize {
    /// Request this many contracts, in 0.01-contract increments.
    Contracts(Decimal),
    /// Request a target cost in dollars; the exchange derives the contract count from it.
    TargetCost(Decimal),
}

/// One child RFQ of a batch create: the combo to request quotes on, optionally sized on its own.
///
/// Set exactly one of [`Self::market_ticker`] and [`Self::selected_markets`].
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub struct RfqBatchChild {
    /// Ticker of an existing combo market. Set this or `selected_markets`, not both.
    pub market_ticker: Option<String>,
    /// Legs of the combo, at most 6, looked up or created in `NewRfqBatch::mve_collection_ticker`.
    pub selected_markets: Option<Vec<TickerPair>>,
    /// This child's own size, in place of the batch's size.
    pub size: Option<RfqChildSize>,
}

#[cfg(feature = "trading")]
impl Serialize for RfqBatchChild {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("RfqBatchChild", 4)?;
        if let Some(market_ticker) = &self.market_ticker {
            state.serialize_field("market_ticker", market_ticker)?;
        }
        if let Some(selected_markets) = &self.selected_markets {
            state.serialize_field("selected_markets", selected_markets)?;
        }
        match &self.size {
            None => {}
            Some(RfqChildSize::Contracts(contracts)) => {
                state.serialize_field("contracts_fp", &contracts.to_string())?;
            }
            Some(RfqChildSize::TargetCost(dollars)) => {
                state.serialize_field("target_cost_dollars", &dollars.to_string())?;
            }
        }
        state.end()
    }
}

/// Size of an RFQ batch, for every child that sets no size of its own: a contract count or a
/// target cost, exactly one of the two.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub enum RfqBatchSize {
    /// Request this many contracts on every unsized child, in 0.01-contract increments.
    Contracts(Decimal),
    /// Request a target cost on every unsized child; the exchange derives the contract count.
    TargetCost {
        /// Target cost in dollars.
        dollars: Decimal,
        /// Size quotes against the target cost as principal only, with taker fees charged on top.
        excludes_fees: bool,
    },
}

/// A new RFQ batch for `POST /communications/rfq_batches`: one child RFQ per combo, quoted as a
/// unit that fills all-or-nothing.
///
/// A batch holds 2 to 57 child RFQs. Each child is sized by its own size, or by the batch's
/// `size` when it has none; every child must be sized the same way, all by contracts or all by
/// target cost. `rest_remainder` is always sent as `false`: a batch executes as one fill-or-kill,
/// so no child rests a remainder.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq)]
pub struct NewRfqBatch {
    /// The combos to request quotes on, one child RFQ each; 2 to 57 distinct combos.
    pub children: Vec<RfqBatchChild>,
    /// Collection in which children given by `selected_markets` are looked up or created;
    /// required when any child uses `selected_markets`.
    pub mve_collection_ticker: Option<String>,
    /// Size for every child that sets no size of its own; required unless every child is sized.
    pub size: Option<RfqBatchSize>,
    /// Close the creator's oldest open RFQ batches so this one fits under the open-batch limit.
    pub replace_existing: bool,
    /// Hide the creator's communications id from other users until execution.
    pub obscure_creator_id: bool,
    /// Caller-chosen key, unique among the creator's batches including those closed within the
    /// last seven days. Reusing one returns HTTP 409.
    pub client_batch_id: Option<String>,
    /// Subtrader to create the batch for (FCM members only).
    pub subtrader_id: Option<String>,
    /// Subaccount to create the batch for, 0 for the primary account.
    pub subaccount: Option<u32>,
}

#[cfg(feature = "trading")]
impl Serialize for NewRfqBatch {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("NewRfqBatch", 8)?;
        state.serialize_field("children", &self.children)?;
        if let Some(mve_collection_ticker) = &self.mve_collection_ticker {
            state.serialize_field("mve_collection_ticker", mve_collection_ticker)?;
        }
        match &self.size {
            None => {}
            Some(RfqBatchSize::Contracts(contracts)) => {
                state.serialize_field("contracts_fp", &contracts.to_string())?;
            }
            Some(RfqBatchSize::TargetCost {
                dollars,
                excludes_fees,
            }) => {
                state.serialize_field("target_cost_dollars", &dollars.to_string())?;
                state.serialize_field("target_cost_excludes_fees", excludes_fees)?;
            }
        }
        state.serialize_field("rest_remainder", &false)?;
        if self.replace_existing {
            state.serialize_field("replace_existing", &true)?;
        }
        if self.obscure_creator_id {
            state.serialize_field("obscure_creator_id", &true)?;
        }
        if let Some(client_batch_id) = &self.client_batch_id {
            state.serialize_field("client_batch_id", client_batch_id)?;
        }
        if let Some(subtrader_id) = &self.subtrader_id {
            state.serialize_field("subtrader_id", subtrader_id)?;
        }
        if let Some(subaccount) = self.subaccount {
            state.serialize_field("subaccount", &subaccount)?;
        }
        state.end()
    }
}

/// The minimum acceptable bid on one child when accepting an RFQ batch.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RfqBatchAcceptLimit {
    /// UUID of the child RFQ; pass it unchanged.
    pub rfq_id: String,
    /// The lowest bid taken on this child on the accepted side, in dollars.
    #[serde(serialize_with = "decimal_as_string")]
    pub bid_dollars: Decimal,
}

/// An accept of an RFQ batch, for `PUT /communications/rfq_batches/{rfq_batch_id}/accept`.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, Serialize)]
pub struct AcceptRfqBatch {
    /// The side to accept on every child.
    pub accepted_side: AcceptedSide,
    /// Price protection, exactly one entry per child in batch order.
    pub min_bids: Vec<RfqBatchAcceptLimit>,
    /// Idempotency key; retrying with the same id returns the result of the accept that
    /// succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
}

/// The result of accepting an RFQ batch.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AcceptedRfqBatch {
    /// The side accepted on every child.
    pub accepted_side: AcceptedSide,
    /// The quote each child was accepted from, in batch order.
    pub quotes: Vec<Quote>,
}

/// The bid on one child RFQ of a batch quote.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RfqBatchQuoteEntry {
    /// UUID of the child RFQ to price; pass it unchanged.
    pub rfq_id: String,
    /// Bid price for YES contracts on this child, in dollars.
    #[serde(serialize_with = "decimal_as_string")]
    pub yes_bid: Decimal,
    /// Bid price for NO contracts on this child, in dollars.
    #[serde(serialize_with = "decimal_as_string")]
    pub no_bid: Decimal,
}

/// A batch quote to create, for `POST /communications/rfq_batches/{rfq_batch_id}/quotes`.
///
/// A batch quote prices every child of the batch, one entry per child. A new batch quote
/// replaces the maker's previous one on the same batch, and a child resent at an unchanged price
/// keeps its time priority.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, Serialize)]
pub struct NewRfqBatchQuote {
    /// Prices for every child RFQ of the batch, one entry per child in batch order; 2 to 57.
    pub quotes: Vec<RfqBatchQuoteEntry>,
    /// Whether to rest the remainder of each child quote after execution.
    pub rest_remainder: bool,
    /// The maker's resting orders are cancelled rather than crossed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_only: bool,
    /// Subaccount to place the batch quote under, 0 for the primary account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
}

/// The id of a newly created batch quote, as returned by the batch quote create endpoint.
#[cfg(feature = "trading")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreatedRfqBatchQuote {
    /// UUID of the created batch quote; pass it unchanged to the delete and confirm calls.
    #[serde(rename = "id")]
    pub batch_quote_id: String,
}

#[cfg(feature = "trading")]
#[derive(Deserialize)]
struct CreateRfqBatchResponse {
    rfq_batch: RfqBatch,
}

#[cfg(feature = "trading")]
impl TradingClient {
    /// `POST /communications/rfq_batches`: create an RFQ batch — one child RFQ per combo, quoted
    /// as a unit that fills all-or-nothing.
    ///
    /// Billed [`DEFAULT_COST`] tokens per child, so the whole batch must fit in the Write bucket
    /// at once or it is rejected locally before any request is sent. With `client_batch_id` set
    /// the request is marked idempotent: a retry with the same key returns HTTP 409 (the batch
    /// already exists) rather than a duplicate. Without it, a failure after dispatch surfaces as
    /// [`crate::KalshiError::Ambiguous`] because a batch may have been created. Charged to the
    /// Write bucket (the API serves communications endpoints from exchange shard 1, which this
    /// crate models as the single Write bucket).
    pub async fn create_rfq_batch(&self, batch: &NewRfqBatch) -> Result<WriteOutcome<RfqBatch>> {
        let mut request = Request::post("/communications/rfq_batches")
            .body(batch)
            .authenticated()
            .cost(batch_cost(batch.children.len(), DEFAULT_COST));
        if batch.client_batch_id.is_some() {
            request = request.idempotent(true);
        }
        let outcome: WriteOutcome<CreateRfqBatchResponse> =
            self.write(request, String::new(), None).await?;
        Ok(match outcome {
            WriteOutcome::Sent(response) => WriteOutcome::Sent(response.rfq_batch),
            WriteOutcome::DryRun(dry) => WriteOutcome::DryRun(dry),
        })
    }

    /// `DELETE /communications/rfq_batches/{rfq_batch_id}`: close a batch and all of its child
    /// RFQs; open batch quotes on it are cancelled. An accepted batch can no longer be closed.
    /// Idempotent by method.
    pub async fn delete_rfq_batch(&self, rfq_batch_id: &str) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!("/communications/rfq_batches/{rfq_batch_id}")).authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /communications/rfq_batches/{rfq_batch_id}/accept`: accept every child of a batch on
    /// one side, each from the maker quoting it best on that side. Either every child is accepted
    /// or none is, and the balance must cover all of them. `min_bids` protects price: a child
    /// whose best bid on the accepted side is now below its entry fails the accept. Idempotent by
    /// method; with `client_order_id` set, a retry returns the result of the accept that
    /// succeeded.
    pub async fn accept_rfq_batch(
        &self,
        rfq_batch_id: &str,
        accept: &AcceptRfqBatch,
    ) -> Result<WriteOutcome<AcceptedRfqBatch>> {
        let request = Request::put(format!("/communications/rfq_batches/{rfq_batch_id}/accept"))
            .body(accept)
            .authenticated();
        self.write(request, String::new(), accept.client_order_id.clone())
            .await
    }

    /// `POST /communications/rfq_batches/{rfq_batch_id}/quotes`: quote every child of a batch,
    /// each exactly once.
    ///
    /// Billed [`DEFAULT_COST`] tokens per child. A new batch quote replaces the maker's previous
    /// one on the same batch, so a retry cannot create a duplicate: the request is marked
    /// idempotent. A batch quote can win children only within 30 seconds of when it was last
    /// sent, so resend it to stay eligible. Charged to the Write bucket.
    pub async fn create_rfq_batch_quote(
        &self,
        rfq_batch_id: &str,
        quote: &NewRfqBatchQuote,
    ) -> Result<WriteOutcome<CreatedRfqBatchQuote>> {
        let request = Request::post(format!("/communications/rfq_batches/{rfq_batch_id}/quotes"))
            .body(quote)
            .authenticated()
            .idempotent(true)
            .cost(batch_cost(quote.quotes.len(), DEFAULT_COST));
        self.write(request, String::new(), None).await
    }

    /// `DELETE /communications/rfq_batches/{rfq_batch_id}/quotes/{batch_quote_id}`: withdraw one
    /// of the maker's open batch quotes; it can no longer be accepted. Idempotent by method.
    pub async fn delete_rfq_batch_quote(
        &self,
        rfq_batch_id: &str,
        batch_quote_id: &str,
    ) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::delete(format!(
                "/communications/rfq_batches/{rfq_batch_id}/quotes/{batch_quote_id}"
            ))
            .authenticated(),
            String::new(),
            None,
        )
        .await
    }

    /// `PUT /communications/rfq_batches/{rfq_batch_id}/quotes/{batch_quote_id}/confirm`: confirm
    /// the children the batch quote won, all at once. Nothing executes until every winning maker
    /// of the batch has confirmed; a missed confirmation closes the whole batch without trading.
    /// Idempotent by method.
    pub async fn confirm_rfq_batch_quote(
        &self,
        rfq_batch_id: &str,
        batch_quote_id: &str,
    ) -> Result<WriteOutcome<()>> {
        self.write_empty(
            Request::put(format!(
                "/communications/rfq_batches/{rfq_batch_id}/quotes/{batch_quote_id}/confirm"
            ))
            .authenticated(),
            String::new(),
            None,
        )
        .await
    }
}
