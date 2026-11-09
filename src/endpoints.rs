use futures::TryStreamExt;

use crate::client::{Client, Request};
use crate::error::Result;
use crate::rate_limit::CF_BENCHMARKS_COST;
use crate::types::{
    AccountLimits, BatchCandlesticksResponse, Candlestick, CandlesticksResponse,
    CfBenchmarksResponse, EndpointCosts, Event, EventFeeChange, EventResponse, Market,
    MarketCandlesticks, MarketResponse, Orderbook, OrderbookResponse, Series, SeriesFeeChange,
    SeriesFeeChangesResponse, SeriesListResponse, SeriesResponse, Trade,
};

/// Maximum tickers Kalshi accepts in one batch candlestick request.
pub const BATCH_CANDLESTICK_TICKERS: usize = 100;

/// Candlestick period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

impl Period {
    /// Length of the period in minutes, as Kalshi's `period_interval` expects.
    pub fn minutes(self) -> u32 {
        match self {
            Self::Minute => 1,
            Self::Hour => 60,
            Self::Day => 1440,
        }
    }
}

/// Market lifecycle filter accepted by `GET /markets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketStatus {
    /// Not yet open for trading.
    Unopened,
    /// Open for trading.
    Open,
    /// Trading closed, not settled.
    Closed,
    /// Settled.
    Settled,
}

impl MarketStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unopened => "unopened",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Settled => "settled",
        }
    }
}

/// Filters for `GET /markets` and `GET /historical/markets`.
///
/// Kalshi allows only one status and treats timestamp filters as mutually exclusive with some
/// statuses; the historical endpoint honours only `series_ticker` and `event_ticker`.
#[derive(Debug, Clone, Default)]
pub struct MarketFilter {
    /// Restrict to one lifecycle status.
    pub status: Option<MarketStatus>,
    /// Restrict to one series.
    pub series_ticker: Option<String>,
    /// Restrict to one event.
    pub event_ticker: Option<String>,
    /// Earliest close time, Unix seconds.
    pub min_close_ts: Option<i64>,
    /// Latest close time, Unix seconds.
    pub max_close_ts: Option<i64>,
    /// Earliest settlement time, Unix seconds.
    pub min_settled_ts: Option<i64>,
    /// Latest settlement time, Unix seconds.
    pub max_settled_ts: Option<i64>,
}

impl MarketFilter {
    fn apply(&self, path: &str, historical: bool) -> Request {
        let request = Request::get(path)
            .query_opt("series_ticker", self.series_ticker.as_deref())
            .query_opt("event_ticker", self.event_ticker.as_deref());
        if historical {
            return request;
        }
        request
            .query_opt("status", self.status.map(MarketStatus::as_str))
            .query_opt("min_close_ts", self.min_close_ts)
            .query_opt("max_close_ts", self.max_close_ts)
            .query_opt("min_settled_ts", self.min_settled_ts)
            .query_opt("max_settled_ts", self.max_settled_ts)
    }
}

impl Client {
    /// `GET /series/{series_ticker}`: the series with its current fee type and multiplier.
    pub async fn series(&self, series_ticker: &str) -> Result<Series> {
        let response: SeriesResponse = self
            .send(&Request::get(format!("/series/{series_ticker}")))
            .await?;
        Ok(response.series)
    }

    /// `GET /series`: every series, with current fees. Not paginated by Kalshi.
    pub async fn list_series(&self) -> Result<Vec<Series>> {
        let response: SeriesListResponse = self.send(&Request::get("/series")).await?;
        Ok(response.series)
    }

    /// `GET /series/fee_changes`: scheduled series fee changes. `show_historical` includes past ones.
    pub async fn series_fee_changes(
        &self,
        series_ticker: Option<&str>,
        show_historical: bool,
    ) -> Result<Vec<SeriesFeeChange>> {
        let request = Request::get("/series/fee_changes")
            .query_opt("series_ticker", series_ticker)
            .query("show_historical", show_historical);
        let response: SeriesFeeChangesResponse = self.send(&request).await?;
        Ok(response.series_fee_change_arr)
    }

    /// `GET /events/fee_changes`: scheduled event fee overrides.
    pub async fn event_fee_changes(
        &self,
        event_ticker: Option<&str>,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<EventFeeChange>> {
        let request = Request::get("/events/fee_changes").query_opt("event_ticker", event_ticker);
        self.paginate(request, "event_fee_changes", limit, max_items)
            .try_collect()
            .await
    }

    /// `GET /events`: events, optionally with nested markets.
    pub async fn events(
        &self,
        with_nested_markets: bool,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Event>> {
        let request = Request::get("/events").query("with_nested_markets", with_nested_markets);
        self.paginate(request, "events", limit, max_items)
            .try_collect()
            .await
    }

    /// `GET /events/{event_ticker}`: one event with its markets.
    pub async fn event(&self, event_ticker: &str) -> Result<EventResponse> {
        self.send(&Request::get(format!("/events/{event_ticker}")))
            .await
    }

    /// `GET /markets`: live markets matching `filter`.
    pub async fn markets(
        &self,
        filter: &MarketFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Market>> {
        self.paginate(filter.apply("/markets", false), "markets", limit, max_items)
            .try_collect()
            .await
    }

    /// `GET /historical/markets`: archived markets matching `filter`.
    pub async fn historical_markets(
        &self,
        filter: &MarketFilter,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Market>> {
        self.paginate(
            filter.apply("/historical/markets", true),
            "markets",
            limit,
            max_items,
        )
        .try_collect()
        .await
    }

    /// One market, live or archived (see [`Client::market`]).
    pub async fn get_market(&self, ticker: &str) -> Result<Market> {
        let response: MarketResponse = self.market(ticker).await?;
        Ok(response.market)
    }

    /// Candlesticks for `[start_ts, end_ts]` (Unix seconds), from the live or historical endpoint
    /// depending on when the market settled.
    pub async fn get_candlesticks(
        &self,
        series_ticker: &str,
        ticker: &str,
        start_ts: i64,
        end_ts: i64,
        period: Period,
    ) -> Result<Vec<Candlestick>> {
        let settled_at = self.get_market(ticker).await?.settlement_ts;
        let response: CandlesticksResponse = self
            .candlesticks(
                series_ticker,
                ticker,
                settled_at,
                start_ts,
                end_ts,
                period.minutes(),
            )
            .await?;
        Ok(response.candlesticks)
    }

    /// `GET /markets/candlesticks`: live candlesticks for many markets, requested in groups of
    /// [`BATCH_CANDLESTICK_TICKERS`].
    pub async fn batch_candlesticks(
        &self,
        tickers: &[&str],
        start_ts: i64,
        end_ts: i64,
        period: Period,
        include_latest_before_start: bool,
    ) -> Result<Vec<MarketCandlesticks>> {
        let mut markets = Vec::new();
        for group in tickers.chunks(BATCH_CANDLESTICK_TICKERS) {
            let request = Request::get("/markets/candlesticks")
                .query("market_tickers", group.join(","))
                .query("start_ts", start_ts)
                .query("end_ts", end_ts)
                .query("period_interval", period.minutes())
                .query("include_latest_before_start", include_latest_before_start);
            let response: BatchCandlesticksResponse = self.send(&request).await?;
            markets.extend(response.markets);
        }
        Ok(markets)
    }

    /// Trades in `[min_ts, max_ts]` (Unix seconds), merged across the live and historical endpoints
    /// (see [`Client::trades`]).
    pub async fn get_trades(
        &self,
        ticker: Option<&str>,
        min_ts: i64,
        max_ts: i64,
        limit: u32,
        max_items: usize,
    ) -> Result<Vec<Trade>> {
        self.trades(ticker, min_ts, max_ts, limit, max_items).await
    }

    /// `GET /markets/{ticker}/orderbook`: the current snapshot; Kalshi keeps no order book history.
    /// `depth` 0 returns every level.
    pub async fn orderbook(&self, ticker: &str, depth: u32) -> Result<Orderbook> {
        let request = Request::get(format!("/markets/{ticker}/orderbook")).query("depth", depth);
        let response: OrderbookResponse = self.send(&request).await?;
        Ok(response.orderbook_fp)
    }

    /// `GET /cfbenchmarks/values`: latest CF Benchmarks values. Needs credentials and an account
    /// entitlement; costs [`CF_BENCHMARKS_COST`] tokens. `query` is forwarded upstream (for example `id`).
    pub async fn cf_benchmarks_values(
        &self,
        query: &[(&str, &str)],
    ) -> Result<CfBenchmarksResponse> {
        self.cf_benchmarks("/cfbenchmarks/values", query).await
    }

    /// `GET /cfbenchmarks/history/values`: tick-level CF Benchmarks history. Same requirements as
    /// [`Client::cf_benchmarks_values`].
    pub async fn cf_benchmarks_history_values(
        &self,
        query: &[(&str, &str)],
    ) -> Result<CfBenchmarksResponse> {
        self.cf_benchmarks("/cfbenchmarks/history/values", query)
            .await
    }

    async fn cf_benchmarks(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<CfBenchmarksResponse> {
        let request = query
            .iter()
            .fold(Request::get(path), |request, (key, value)| {
                request.query(key, value)
            })
            .cost(CF_BENCHMARKS_COST)
            .authenticated();
        self.send(&request).await
    }

    /// `GET /account/limits`: the account's usage tier, Read and Write bucket limits, and grants.
    /// Needs credentials. `usage_tier` is the tier to size a client with for this account.
    pub async fn account_limits(&self) -> Result<AccountLimits> {
        self.send(&Request::get("/account/limits").authenticated())
            .await
    }

    /// `GET /account/endpoint_costs`: the default token cost and every endpoint whose cost
    /// differs from it. Needs credentials.
    pub async fn account_endpoint_costs(&self) -> Result<EndpointCosts> {
        self.send(&Request::get("/account/endpoint_costs").authenticated())
            .await
    }
}

impl MarketFilter {
    /// A filter for every market of one series.
    pub fn series(series_ticker: impl Into<String>) -> Self {
        Self {
            series_ticker: Some(series_ticker.into()),
            ..Self::default()
        }
    }
}
