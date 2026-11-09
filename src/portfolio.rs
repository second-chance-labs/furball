use futures::TryStreamExt;

use crate::client::{Client, Request};
use crate::error::Result;
use crate::types::{
    Balance, Positions, PositionsPage, Settlement, SubaccountBalance, SubaccountBalancesResponse,
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
        let mut positions = Positions::default();
        for page in pages {
            positions.extend(page.market_positions, page.event_positions);
        }
        Ok(positions)
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
}

/// Keeps at most `max_items` positions, in page order: a page's market positions precede its event
/// positions.
fn truncate_pages(pages: &mut Vec<PositionsPage>, max_items: usize) {
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
