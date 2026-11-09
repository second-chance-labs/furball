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
use crate::trading::{TradingClient, WriteOutcome};
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
