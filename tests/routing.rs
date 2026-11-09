mod common;

use chrono::{TimeZone, Utc};
use common::{API_PREFIX, Trade, client};
use furball::KalshiError;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CUTOFF_SECS: i64 = 1_700_000_000;

fn cutoff_body() -> Value {
    let iso = Utc.timestamp_opt(CUTOFF_SECS, 0).unwrap().to_rfc3339();
    json!({
        "market_settled_ts": iso,
        "trades_created_ts": iso,
        "orders_updated_ts": iso,
        "market_positions_last_updated_ts": iso
    })
}

async fn mount_cutoff(server: &MockServer, expected_calls: u64) {
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/cutoff")))
        .respond_with(ResponseTemplate::new(200).set_body_json(cutoff_body()))
        .expect(expected_calls)
        .mount(server)
        .await;
}

async fn mount_json(server: &MockServer, route: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}{route}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn candlesticks_for_markets_settled_before_cutoff_use_historical_endpoint() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    mount_json(
        &server,
        "/historical/markets/M1/candlesticks",
        json!({ "source": "historical" }),
    )
    .await;
    let settled = Utc.timestamp_opt(CUTOFF_SECS - 1, 0).unwrap();
    let value: Value = client(&server, false)
        .candlesticks("S", "M1", Some(settled), 1, 2, 60)
        .await
        .expect("ok");
    assert_eq!(value["source"], "historical");
}

#[tokio::test]
async fn candlesticks_for_recent_or_unsettled_markets_use_live_endpoint() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/series/S/markets/M1/candlesticks"
        )))
        .and(query_param("period_interval", "60"))
        .and(query_param("start_ts", "1"))
        .and(query_param("end_ts", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "source": "live" })))
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server, false);
    let settled = Utc.timestamp_opt(CUTOFF_SECS, 0).unwrap();
    let recent: Value = client
        .candlesticks("S", "M1", Some(settled), 1, 2, 60)
        .await
        .expect("recent");
    let unsettled: Value = client
        .candlesticks("S", "M1", None, 1, 2, 60)
        .await
        .expect("unsettled");
    assert_eq!(recent["source"], "live");
    assert_eq!(unsettled["source"], "live");
}

#[tokio::test]
async fn market_falls_back_to_historical_only_when_live_returns_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/markets/OLD")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_json(
        &server,
        "/historical/markets/OLD",
        json!({ "source": "historical" }),
    )
    .await;
    mount_json(&server, "/markets/NEW", json!({ "source": "live" })).await;
    let client = client(&server, false);
    assert_eq!(
        client.market::<Value>("OLD").await.expect("old")["source"],
        "historical"
    );
    assert_eq!(
        client.market::<Value>("NEW").await.expect("new")["source"],
        "live"
    );
    assert!(matches!(
        client.market::<Value>("GONE").await,
        Err(KalshiError::NotFound)
    ));
}

fn trades(ids: &[&str]) -> Value {
    json!({ "trades": ids.iter().map(|id| json!({ "trade_id": id })).collect::<Vec<_>>(), "cursor": "" })
}

#[tokio::test]
async fn trades_spanning_the_cutoff_are_split_merged_and_deduplicated() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    let historical_max = CUTOFF_SECS.to_string();
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/trades")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", historical_max.as_str()))
        .and(query_param("ticker", "M1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(trades(&["t1", "t2", "t3"])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/markets/trades")))
        .and(query_param("min_ts", historical_max.as_str()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(trades(&["t3", "t4"])))
        .expect(1)
        .mount(&server)
        .await;
    let merged: Vec<Trade> = client(&server, false)
        .trades(Some("M1"), CUTOFF_SECS - 100, CUTOFF_SECS + 100, 100, 1000)
        .await
        .expect("trades");
    let ids: Vec<_> = merged.iter().map(|trade| trade.trade_id.as_str()).collect();
    assert_eq!(ids, ["t1", "t2", "t3", "t4"]);
}

#[tokio::test]
async fn trades_entirely_before_or_after_cutoff_hit_a_single_endpoint() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    mount_json(&server, "/historical/trades", trades(&["old"])).await;
    mount_json(&server, "/markets/trades", trades(&["new"])).await;
    let client = client(&server, false);
    let old: Vec<Trade> = client
        .trades(None, CUTOFF_SECS - 200, CUTOFF_SECS - 100, 100, 10)
        .await
        .expect("old");
    let new: Vec<Trade> = client
        .trades(None, CUTOFF_SECS + 100, CUTOFF_SECS + 200, 100, 10)
        .await
        .expect("new");
    assert_eq!(old[0].trade_id, "old");
    assert_eq!(new[0].trade_id, "new");
}

#[tokio::test]
async fn cutoff_is_cached_until_ttl_expires() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 2).await;
    let client = furball::Client::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .cutoff_ttl(std::time::Duration::from_millis(50))
        .build()
        .expect("client");
    let first = client.cutoff().await.expect("first");
    assert_eq!(client.cutoff().await.expect("cached"), first);
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    client.cutoff().await.expect("refreshed");
}
