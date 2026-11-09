use futures::TryStreamExt;
use rust_decimal::Decimal;

use crate::client::{Client, Request};
use crate::error::Result;
use crate::rate_limit::GET_ORDER_COST;
use crate::types::{
    Balance, Order, OrderQueuePosition, OrderQueuePositionResponse, OrderQueuePositionsResponse,
    OrderResponse, OrderStatus, Positions, PositionsPage, Settlement, SubaccountBalance,
    SubaccountBalancesResponse,
};

/// Settlement state filter for portfolio positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementStatus {
    /// Not yet settled; the API default.
    Unsettled,
    /// Settled, not yet archived.
    Settled,
    /// Both states.
    All,
}

impl SettlementStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unsettled => "unsettled",
            Self::Settled => "settled",
            Self::All => "all",
        }
    }
}

/// Filters for `GET /portfolio/positions`.
///
/// `settlement_status` defaults to the API default, unsettled; pass
/// [`SettlementStatus::Settled`] or [`SettlementStatus::All`] to include settled positions.
#[derive(Debug, Clone, Default)]
pub struct PositionFilter {
    /// Settlement state: unsettled (API default), settled, or both.
    pub settlement_status: Option<SettlementStatus>,
    /// Count filter: `position` and/or `total_traded`, comma-separated.
    pub count_filter: Option<String>,
    /// Market ticker.
    pub ticker: Option<String>,
    /// Event ticker; only a single event is supported.
    pub event_ticker: Option<String>,
    /// Subaccount number, 0 for the primary account; omitted uses the key's default.
    pub subaccount: Option<u32>,
    /// Exchange shard; omitted returns all shards.
    pub exchange_index: Option<u32>,
}

impl PositionFilter {
    fn apply(&self) -> Request {
        Request::get("/portfolio/positions")
            .query_opt(
                "settlement_status",
                self.settlement_status.map(SettlementStatus::as_str),
            )
            .query_opt("count_filter", self.count_filter.as_deref())
            .query_opt("ticker", self.ticker.as_deref())
            .query_opt("event_ticker", self.event_ticker.as_deref())
            .query_opt("subaccount", self.subaccount)
            .query_opt("exchange_index", self.exchange_index)
    }
}

/// Filters for `GET /portfolio/settlements`.
#[derive(Debug, Clone, Default)]
pub struct SettlementFilter {
    /// Market ticker.
    pub ticker: Option<String>,
    /// Event ticker; only a single event is supported.
    pub event_ticker: Option<String>,
    /// Settlements after this Unix timestamp.
    pub min_ts: Option<i64>,
    /// Settlements before this Unix timestamp.
    pub max_ts: Option<i64>,
    /// Subaccount number; omitted returns all subaccounts.
    pub subaccount: Option<u32>,
}

impl SettlementFilter {
    fn apply(&self) -> Request {
        Request::get("/portfolio/settlements")
            .query_opt("ticker", self.ticker.as_deref())
            .query_opt("event_ticker", self.event_ticker.as_deref())
            .query_opt("min_ts", self.min_ts)
            .query_opt("max_ts", self.max_ts)
            .query_opt("subaccount", self.subaccount)
            .authenticated()
    }
}

/// Filters for the order endpoints, live and historical.
///
/// `min_ts` and `max_ts` are Unix seconds; a `None` bound is unbounded. When the range spans the
/// historical cutoff, orders are read from both endpoints and merged.
#[derive(Debug, Clone, Default)]
pub struct OrderFilter {
    /// Market ticker.
    pub ticker: Option<String>,
    /// Earliest order update time.
    pub min_ts: Option<i64>,
    /// Latest order update time.
    pub max_ts: Option<i64>,
    /// Order status; `Resting` reads only the live endpoint, other statuses are filtered
    /// client-side on the historical leg, which has no `status` parameter.
    pub status: Option<OrderStatus>,
    /// Subaccount number; omitted returns all subaccounts.
    pub subaccount: Option<u32>,
}

/// Filters for the fills endpoints, live and historical.
///
/// `min_ts` and `max_ts` are Unix seconds; a `None` bound is unbounded. When the range spans the
/// historical cutoff, fills are read from both endpoints and merged.
#[derive(Debug, Clone, Default)]
pub struct FillFilter {
    /// Market ticker.
    pub ticker: Option<String>,
    /// Order that produced the fills; the historical endpoint has no `order_id` parameter, so this
    /// is filtered client-side on the historical leg.
    pub order_id: Option<String>,
    /// Earliest fill time.
    pub min_ts: Option<i64>,
    /// Latest fill time.
    pub max_ts: Option<i64>,
    /// Subaccount number; omitted returns all subaccounts.
    pub subaccount: Option<u32>,
}

impl Client {
    /// `GET /portfolio/balance`: balance and portfolio value for the primary account or one
    /// subaccount. Throttled by the portfolio balance bucket, not the Read budget.
    pub async fn balance(
        &self,
        subaccount: Option<u32>,
        exchange_index: Option<u32>,
    ) -> Result<Balance> {
        let request = Request::get("/portfolio/balance")
            .query_opt("subaccount", subaccount)
            .query_opt("exchange_index", exchange_index)
            .balance()
            .authenticated();
        self.send(&request).await
    }

    /// `GET /portfolio/subaccounts/balances`: balance of every subaccount, including the primary
    /// account.
    pub async fn subaccount_balances(&self) -> Result<Vec<SubaccountBalance>> {
        let response: SubaccountBalancesResponse = self
            .send(&Request::get("/portfolio/subaccounts/balances").authenticated())
            .await?;
        Ok(response.subaccount_balances)
    }

    /// `GET /portfolio/positions`: market and event positions, following cursors until the list is
    /// exhausted or at most `max_items` positions have been collected.
    pub async fn positions(
        &self,
        filter: &PositionFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Positions> {
        let pages = self.position_pages(filter, limit, max_items).await?;
        Ok(flatten_pages(pages))
    }

    pub(crate) async fn position_pages(
        &self,
        filter: &PositionFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<PositionsPage>> {
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let request = filter
                .apply()
                .query("limit", limit)
                .query_opt("cursor", cursor.as_deref())
                .authenticated();
            let page: PositionsPage = self.send(&request).await?;
            pages.push(page);
            let collected: usize = pages
                .iter()
                .map(|page| page.market_positions.len() + page.event_positions.len())
                .sum();
            if collected >= max_items {
                truncate_pages(&mut pages, max_items);
                break;
            }
            let Some(next) = pages
                .last()
                .and_then(|page| page.cursor.as_deref())
                .filter(|cursor| !cursor.is_empty())
            else {
                break;
            };
            cursor = Some(next.to_owned());
        }
        Ok(pages)
    }

    /// `GET /portfolio/settlements`: settlement records still in the live data set.
    pub async fn settlements(
        &self,
        filter: &SettlementFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Settlement>> {
        self.paginate(filter.apply(), "settlements", limit, max_items)
            .try_collect()
            .await
    }

    /// `GET /portfolio/orders/{order_id}`: one order, live or completed. Costs
    /// [`GET_ORDER_COST`] tokens.
    pub async fn get_order(&self, order_id: &str) -> Result<Order> {
        let request = Request::get(format!("/portfolio/orders/{order_id}"))
            .cost(GET_ORDER_COST)
            .authenticated();
        let response: OrderResponse = self.send(&request).await?;
        Ok(response.order)
    }

    /// `GET /portfolio/orders/{order_id}/queue_position`: contracts ahead of one resting order in
    /// the book, under price-time priority.
    pub async fn order_queue_position(&self, order_id: &str) -> Result<Decimal> {
        let request =
            Request::get(format!("/portfolio/orders/{order_id}/queue_position")).authenticated();
        let response: OrderQueuePositionResponse = self.send(&request).await?;
        Ok(response.queue_position_fp)
    }

    /// `GET /portfolio/orders/queue_positions`: queue positions of all resting orders, optionally
    /// filtered by market tickers (comma-separated upstream) or one event.
    pub async fn order_queue_positions(
        &self,
        market_tickers: &[&str],
        event_ticker: Option<&str>,
        subaccount: Option<u32>,
    ) -> Result<Vec<OrderQueuePosition>> {
        let mut request = Request::get("/portfolio/orders/queue_positions")
            .query_opt("event_ticker", event_ticker)
            .query_opt("subaccount", subaccount)
            .authenticated();
        if !market_tickers.is_empty() {
            request = request.query("market_tickers", market_tickers.join(","));
        }
        let response: OrderQueuePositionsResponse = self.send(&request).await?;
        Ok(response.queue_positions)
    }
}

pub(crate) fn flatten_pages(pages: Vec<PositionsPage>) -> Positions {
    let mut positions = Positions::default();
    for page in pages {
        positions.extend(page.market_positions, page.event_positions);
    }
    positions
}

/// Keeps at most `max_items` positions, in page order: a page's market positions precede its event
/// positions.
pub(crate) fn truncate_pages(pages: &mut Vec<PositionsPage>, max_items: usize) {
    let mut remaining = max_items;
    for page in pages.iter_mut() {
        if remaining == 0 {
            page.market_positions.clear();
            page.event_positions.clear();
            continue;
        }
        let market = page.market_positions.len();
        if remaining >= market {
            remaining -= market;
        } else {
            page.market_positions.truncate(remaining);
            remaining = 0;
        }
        if remaining > 0 {
            let consumed = page.event_positions.len().min(remaining);
            page.event_positions.truncate(consumed);
            remaining -= consumed;
        } else {
            page.event_positions.clear();
        }
    }
    while pages
        .last()
        .is_some_and(|page| page.market_positions.is_empty() && page.event_positions.is_empty())
    {
        pages.pop();
    }
}
