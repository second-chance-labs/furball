use std::collections::HashSet;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use crate::client::{Client, Request};
use crate::error::{KalshiError, Result};
use crate::portfolio::{
    FillFilter, OrderFilter, PositionFilter, SettlementStatus, flatten_pages, truncate_pages,
};
use crate::types::{Fill, Order, OrderStatus, Positions, PositionsPage};

/// Timestamps from `GET /historical/cutoff` that split live from archived data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Cutoff {
    /// Markets (and candlesticks) that settled before this instant are historical-only.
    pub market_settled_ts: DateTime<Utc>,
    /// Trades filled before this instant are historical-only.
    pub trades_created_ts: DateTime<Utc>,
    /// Orders finished before this instant are historical-only.
    pub orders_updated_ts: DateTime<Utc>,
    /// Backfill horizon for settled positions: a position is visible in
    /// `GET /historical/positions` only after its event completes the handoff, which may lag this
    /// timestamp, so complete position history reads both endpoints.
    pub market_positions_last_updated_ts: DateTime<Utc>,
}

/// A trade type that exposes the identifier used to de-duplicate live and historical results.
pub trait TradeId {
    /// Unique trade identifier.
    fn trade_id(&self) -> &str;
}

/// One side of a cutoff split: which endpoint a listing leg reads from.
struct Leg {
    /// Endpoint path.
    path: &'static str,
    /// The live endpoint, as opposed to the historical archive.
    live: bool,
}

impl Client {
    /// Returns the historical cutoff, refreshing it when the cached copy is older than the configured TTL.
    pub async fn cutoff(&self) -> Result<Cutoff> {
        let mut cached = self.inner.cutoff.lock().await;
        if let Some((fetched_at, cutoff)) = cached.as_ref()
            && fetched_at.elapsed() < self.inner.cutoff_ttl
        {
            return Ok(cutoff.clone());
        }
        let cutoff: Cutoff = self.send(&Request::get("/historical/cutoff")).await?;
        *cached = Some((Instant::now(), cutoff.clone()));
        Ok(cutoff)
    }

    /// Fetches a market, falling back to the historical archive when the live API no longer has it.
    pub async fn market<T: DeserializeOwned>(&self, ticker: &str) -> Result<T> {
        match self.send(&Request::get(format!("/markets/{ticker}"))).await {
            Err(KalshiError::NotFound) => {
                self.send(&Request::get(format!("/historical/markets/{ticker}")))
                    .await
            }
            other => other,
        }
    }

    /// Fetches candlesticks from the live or historical endpoint depending on when the market settled.
    ///
    /// Pass `settled_at = None` for a market that has not settled. `period_minutes` is 1, 60 or 1440.
    pub async fn candlesticks<T: DeserializeOwned>(
        &self,
        series_ticker: &str,
        ticker: &str,
        settled_at: Option<DateTime<Utc>>,
        start_ts: i64,
        end_ts: i64,
        period_minutes: u32,
    ) -> Result<T> {
        let archived = match settled_at {
            Some(settled_at) => settled_at < self.cutoff().await?.market_settled_ts,
            None => false,
        };
        let path = if archived {
            format!("/historical/markets/{ticker}/candlesticks")
        } else {
            format!("/series/{series_ticker}/markets/{ticker}/candlesticks")
        };
        let request = Request::get(path)
            .query("start_ts", start_ts)
            .query("end_ts", end_ts)
            .query("period_interval", period_minutes);
        self.send(&request).await
    }

    /// Fetches trades in `[min_ts, max_ts]` (Unix seconds), splitting at the cutoff between the
    /// historical and live endpoints, then merging and de-duplicating by trade ID (older part first).
    pub async fn trades<T>(
        &self,
        ticker: Option<&str>,
        min_ts: i64,
        max_ts: i64,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned + TradeId + Send + 'static,
    {
        let split = self.cutoff().await?.trades_created_ts.timestamp();
        let mut trades = Vec::new();
        if min_ts <= split {
            trades.extend(
                self.trade_pages::<T>(
                    "/historical/trades",
                    ticker,
                    min_ts,
                    max_ts.min(split),
                    limit,
                    max_items,
                )
                .await?,
            );
        }
        if max_ts >= split {
            trades.extend(
                self.trade_pages::<T>(
                    "/markets/trades",
                    ticker,
                    min_ts.max(split),
                    max_ts,
                    limit,
                    max_items,
                )
                .await?,
            );
        }
        let mut seen = HashSet::new();
        trades.retain(|trade| seen.insert(trade.trade_id().to_owned()));
        trades.truncate(max_items);
        Ok(trades)
    }

    async fn trade_pages<T>(
        &self,
        path: &str,
        ticker: Option<&str>,
        min_ts: i64,
        max_ts: i64,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let request = Request::get(path)
            .query("min_ts", min_ts)
            .query("max_ts", max_ts)
            .query_opt("ticker", ticker);
        self.paginate::<T>(request, "trades", limit, max_items)
            .try_collect()
            .await
    }

    /// Fetches orders matching `filter`, splitting at the historical cutoff between
    /// `GET /historical/orders` and `GET /portfolio/orders`, then merging and de-duplicating by
    /// order ID (older part first). Resting orders never archive, so a `Resting` status reads only
    /// the live endpoint.
    pub async fn orders(
        &self,
        filter: &OrderFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Order>> {
        let split = self.cutoff().await?.orders_updated_ts.timestamp();
        let mut orders = Vec::new();
        if matches!(filter.status, Some(OrderStatus::Resting)) {
            orders.extend(
                self.order_pages(
                    Leg {
                        path: "/portfolio/orders",
                        live: true,
                    },
                    filter,
                    filter.min_ts,
                    filter.max_ts,
                    limit,
                    max_items,
                )
                .await?,
            );
        } else {
            let historical_needed = filter.min_ts.is_none_or(|min| min <= split);
            let live_needed = filter.max_ts.is_none_or(|max| max >= split);
            if historical_needed {
                orders.extend(
                    self.order_pages(
                        Leg {
                            path: "/historical/orders",
                            live: false,
                        },
                        filter,
                        filter.min_ts,
                        filter.max_ts.map(|max| max.min(split)),
                        limit,
                        max_items,
                    )
                    .await?,
                );
            }
            if live_needed {
                orders.extend(
                    self.order_pages(
                        Leg {
                            path: "/portfolio/orders",
                            live: true,
                        },
                        filter,
                        filter.min_ts.map(|min| min.max(split)),
                        filter.max_ts,
                        limit,
                        max_items,
                    )
                    .await?,
                );
            }
        }
        let mut seen = HashSet::new();
        orders.retain(|order| seen.insert(order.order_id.clone()));
        orders.truncate(max_items);
        Ok(orders)
    }

    async fn order_pages(
        &self,
        leg: Leg,
        filter: &OrderFilter,
        min_ts: Option<i64>,
        max_ts: Option<i64>,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Order>> {
        let mut request = Request::get(leg.path)
            .query_opt("ticker", filter.ticker.as_deref())
            .query_opt("min_ts", min_ts)
            .query_opt("max_ts", max_ts)
            .query_opt("subaccount", filter.subaccount)
            .authenticated();
        if leg.live {
            request =
                request.query_opt("status", filter.status.as_ref().map(OrderStatus::as_query));
        }
        let mut orders: Vec<Order> = self
            .paginate::<Order>(request, "orders", limit, max_items)
            .try_collect()
            .await?;
        if !leg.live
            && let Some(status) = filter.status.as_ref()
        {
            orders.retain(|order| order.status == *status);
        }
        Ok(orders)
    }

    /// Fetches fills matching `filter`, splitting at the historical cutoff between
    /// `GET /historical/fills` and `GET /portfolio/fills`, then merging and de-duplicating by fill
    /// ID (older part first).
    pub async fn fills(
        &self,
        filter: &FillFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Fill>> {
        let split = self.cutoff().await?.trades_created_ts.timestamp();
        let mut fills = Vec::new();
        let historical_needed = filter.min_ts.is_none_or(|min| min <= split);
        let live_needed = filter.max_ts.is_none_or(|max| max >= split);
        if historical_needed {
            fills.extend(
                self.fill_pages(
                    Leg {
                        path: "/historical/fills",
                        live: false,
                    },
                    filter,
                    filter.min_ts,
                    filter.max_ts.map(|max| max.min(split)),
                    limit,
                    max_items,
                )
                .await?,
            );
        }
        if live_needed {
            fills.extend(
                self.fill_pages(
                    Leg {
                        path: "/portfolio/fills",
                        live: true,
                    },
                    filter,
                    filter.min_ts.map(|min| min.max(split)),
                    filter.max_ts,
                    limit,
                    max_items,
                )
                .await?,
            );
        }
        let mut seen = HashSet::new();
        fills.retain(|fill| seen.insert(fill.fill_id.clone()));
        fills.truncate(max_items);
        Ok(fills)
    }

    async fn fill_pages(
        &self,
        leg: Leg,
        filter: &FillFilter,
        min_ts: Option<i64>,
        max_ts: Option<i64>,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Fill>> {
        let mut request = Request::get(leg.path)
            .query_opt("ticker", filter.ticker.as_deref())
            .query_opt("min_ts", min_ts)
            .query_opt("max_ts", max_ts)
            .query_opt("subaccount", filter.subaccount)
            .authenticated();
        if leg.live {
            request = request.query_opt("order_id", filter.order_id.as_deref());
        }
        let mut fills: Vec<Fill> = self
            .paginate::<Fill>(request, "fills", limit, max_items)
            .try_collect()
            .await?;
        if !leg.live
            && let Some(order_id) = filter.order_id.as_deref()
        {
            fills.retain(|fill| fill.order_id == order_id);
        }
        Ok(fills)
    }

    /// Fetches the complete position history of one subaccount: live
    /// `GET /portfolio/positions` with `settlement_status=all` first, then archived
    /// `GET /historical/positions`, de-duplicated by ticker (a row present in both reads is kept
    /// from the live read). The reads are not one snapshot; a row that hands off between them can
    /// overlap until the next reconciliation.
    pub async fn position_history(
        &self,
        subaccount: u32,
        limit: u32,
        max_items: usize,
    ) -> Result<Positions> {
        let filter = PositionFilter {
            settlement_status: Some(SettlementStatus::All),
            subaccount: Some(subaccount),
            ..Default::default()
        };
        let mut pages = self.position_pages(&filter, limit, max_items).await?;
        let live_count: usize = pages
            .iter()
            .map(|page| page.market_positions.len() + page.event_positions.len())
            .sum();
        if live_count < max_items {
            let mut cursor: Option<String> = None;
            loop {
                let request = Request::get("/historical/positions")
                    .query("subaccount", subaccount)
                    .query("limit", limit)
                    .query_opt("cursor", cursor.as_deref())
                    .authenticated();
                let mut page: PositionsPage = self.send(&request).await?;
                let market_seen: HashSet<&str> = pages
                    .iter()
                    .flat_map(|page| &page.market_positions)
                    .map(|position| position.ticker.as_str())
                    .collect();
                page.market_positions
                    .retain(|position| !market_seen.contains(position.ticker.as_str()));
                let event_seen: HashSet<&str> = pages
                    .iter()
                    .flat_map(|page| &page.event_positions)
                    .map(|position| position.event_ticker.as_str())
                    .collect();
                page.event_positions
                    .retain(|position| !event_seen.contains(position.event_ticker.as_str()));
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
        }
        Ok(flatten_pages(pages))
    }
}
