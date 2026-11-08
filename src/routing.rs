use std::collections::HashSet;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use crate::client::{Client, Request};
use crate::error::{KalshiError, Result};

/// Timestamps from `GET /historical/cutoff` that split live from archived data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Cutoff {
    /// Markets (and candlesticks) that settled before this instant are historical-only.
    pub market_settled_ts: DateTime<Utc>,
    /// Trades filled before this instant are historical-only.
    pub trades_created_ts: DateTime<Utc>,
    /// Orders finished before this instant are historical-only.
    pub orders_updated_ts: DateTime<Utc>,
}

/// A trade type that exposes the identifier used to de-duplicate live and historical results.
pub trait TradeId {
    /// Unique trade identifier.
    fn trade_id(&self) -> &str;
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
}
