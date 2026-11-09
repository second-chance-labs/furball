mod common;

use chrono::{TimeZone, Utc};
use common::{API_PREFIX, client};
use furball::types::{BookSide, OrderStatus, OutcomeSide};
use furball::{FillFilter, KalshiError, OrderFilter};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use wiremock::matchers::{header_exists, method, path, query_param};
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

fn fixture(name: &str) -> Value {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("fixture json")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal")
}

fn order(id: &str, status: &str) -> Value {
    json!({
        "order_id": id,
        "user_id": "u1",
        "client_order_id": format!("c-{id}"),
        "ticker": "KXHIGHNY-26NOV07-T68",
        "outcome_side": "yes",
        "book_side": "bid",
        "type": "limit",
        "status": status,
        "yes_price_dollars": "0.50",
        "no_price_dollars": "0.50",
        "fill_count_fp": "0.00",
        "remaining_count_fp": "10.00",
        "initial_count_fp": "10.00",
        "taker_fill_cost_dollars": "0.00",
        "maker_fill_cost_dollars": "0.00",
        "taker_fees_dollars": "0.00",
        "maker_fees_dollars": "0.00"
    })
}

fn order_page(orders: Vec<Value>) -> Value {
    json!({ "orders": orders, "cursor": "" })
}

fn fills(ids: &[&str]) -> Value {
    json!({
        "fills": ids.iter().map(|id| json!({
            "fill_id": id,
            "exchange_index": 0,
            "trade_id": id,
            "order_id": format!("o-{id}"),
            "ticker": "KXHIGHNY-26NOV07-T68",
            "market_ticker": "KXHIGHNY-26NOV07-T68",
            "outcome_side": "yes",
            "book_side": "bid",
            "count_fp": "1.00",
            "yes_price_dollars": "0.50",
            "no_price_dollars": "0.50",
            "is_taker": true,
            "fee_cost": "0.0175"
        })).collect::<Vec<_>>(),
        "cursor": ""
    })
}

#[tokio::test]
async fn orders_spanning_cutoff_are_split_merged_and_deduplicated() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/orders")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(|request: &wiremock::Request| {
            let leaked = request.url.query_pairs().any(|(key, _)| key == "status");
            ResponseTemplate::new(if leaked { 400 } else { 200 }).set_body_json(order_page(vec![
                order("o1", "executed"),
                order("o2", "canceled"),
                order("o3", "executed"),
            ]))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(order_page(vec![
            order("o3", "resting"),
            order("o4", "resting"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS - 100),
        max_ts: Some(CUTOFF_SECS + 100),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 1_000)
        .await
        .expect("orders");
    let ids: Vec<_> = orders.iter().map(|order| order.order_id.as_str()).collect();
    assert_eq!(ids, ["o1", "o2", "o3", "o4"]);
}

#[tokio::test]
async fn orders_range_touching_cutoff_hits_both_legs() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/orders")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(order_page(vec![order("ob", "executed")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(order_page(vec![
            order("ob", "resting"),
            order("oa", "resting"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS),
        max_ts: Some(CUTOFF_SECS),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 1_000)
        .await
        .expect("orders");
    let ids: Vec<_> = orders.iter().map(|order| order.order_id.as_str()).collect();
    assert_eq!(ids, ["ob", "oa"]);
}

#[tokio::test]
async fn orders_entirely_before_cutoff_use_historical_only() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/orders")))
        .and(query_param("min_ts", (CUTOFF_SECS - 200).to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS - 100).to_string()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(order_page(vec![order("old", "canceled")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS - 200),
        max_ts: Some(CUTOFF_SECS - 100),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 10)
        .await
        .expect("orders");
    assert_eq!(orders[0].order_id, "old");
}

#[tokio::test]
async fn orders_entirely_after_cutoff_use_live_only() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("min_ts", (CUTOFF_SECS + 100).to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 200).to_string()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(order_page(vec![order("new", "resting")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS + 100),
        max_ts: Some(CUTOFF_SECS + 200),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 10)
        .await
        .expect("orders");
    assert_eq!(orders[0].order_id, "new");
}

#[tokio::test]
async fn resting_orders_read_live_endpoint_only_with_status_parameter() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .and(query_param("status", "resting"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(order_page(vec![order("o-r", "resting")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS - 100),
        max_ts: Some(CUTOFF_SECS + 100),
        status: Some(OrderStatus::Resting),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 1_000)
        .await
        .expect("orders");
    assert_eq!(orders[0].order_id, "o-r");
}

#[tokio::test]
async fn canceled_orders_filter_the_historical_leg_client_side() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/orders")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(|request: &wiremock::Request| {
            let leaked = request.url.query_pairs().any(|(key, _)| key == "status");
            ResponseTemplate::new(if leaked { 400 } else { 200 }).set_body_json(order_page(vec![
                order("o-h-canceled", "canceled"),
                order("o-h-executed", "executed"),
            ]))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .and(query_param("status", "canceled"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(order_page(vec![order("o-l-canceled", "canceled")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS - 100),
        max_ts: Some(CUTOFF_SECS + 100),
        status: Some(OrderStatus::Canceled),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 1_000)
        .await
        .expect("orders");
    let ids: Vec<_> = orders.iter().map(|order| order.order_id.as_str()).collect();
    assert_eq!(ids, ["o-h-canceled", "o-l-canceled"]);
}

#[tokio::test]
async fn fills_spanning_cutoff_are_split_merged_and_deduplicated() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/fills")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills(&["f1", "f2", "f3"])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/fills")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills(&["f3", "f4"])))
        .expect(1)
        .mount(&server)
        .await;
    let filter = FillFilter {
        min_ts: Some(CUTOFF_SECS - 100),
        max_ts: Some(CUTOFF_SECS + 100),
        ..Default::default()
    };
    let merged = client(&server, true)
        .fills(&filter, 100, 1_000)
        .await
        .expect("fills");
    let ids: Vec<_> = merged.iter().map(|fill| fill.fill_id.as_str()).collect();
    assert_eq!(ids, ["f1", "f2", "f3", "f4"]);
}

#[tokio::test]
async fn fills_entirely_before_or_after_cutoff_hit_a_single_endpoint() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/fills")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills(&["old"])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/fills")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills(&["new"])))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let old = client
        .fills(
            &FillFilter {
                min_ts: Some(CUTOFF_SECS - 200),
                max_ts: Some(CUTOFF_SECS - 100),
                ..Default::default()
            },
            100,
            10,
        )
        .await
        .expect("old");
    let new = client
        .fills(
            &FillFilter {
                min_ts: Some(CUTOFF_SECS + 100),
                max_ts: Some(CUTOFF_SECS + 200),
                ..Default::default()
            },
            100,
            10,
        )
        .await
        .expect("new");
    assert_eq!(old[0].fill_id, "old");
    assert_eq!(new[0].fill_id, "new");
}

#[tokio::test]
async fn fills_for_an_order_filter_the_historical_leg_client_side() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/fills")))
        .and(query_param("min_ts", (CUTOFF_SECS - 100).to_string()))
        .and(query_param("max_ts", CUTOFF_SECS.to_string()))
        .respond_with(|request: &wiremock::Request| {
            let leaked = request.url.query_pairs().any(|(key, _)| key == "order_id");
            ResponseTemplate::new(if leaked { 400 } else { 200 })
                .set_body_json(fills(&["keep", "drop"]))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/fills")))
        .and(query_param("min_ts", CUTOFF_SECS.to_string()))
        .and(query_param("max_ts", (CUTOFF_SECS + 100).to_string()))
        .and(query_param("order_id", "o-keep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills(&["keep-live"])))
        .expect(1)
        .mount(&server)
        .await;
    let filter = FillFilter {
        order_id: Some("o-keep".into()),
        min_ts: Some(CUTOFF_SECS - 100),
        max_ts: Some(CUTOFF_SECS + 100),
        ..Default::default()
    };
    let merged = client(&server, true)
        .fills(&filter, 100, 1_000)
        .await
        .expect("fills");
    let ids: Vec<_> = merged.iter().map(|fill| fill.fill_id.as_str()).collect();
    assert_eq!(ids, ["keep", "keep-live"]);
}

#[tokio::test]
async fn orders_list_fixture_decodes_full_fields() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("orders")))
        .expect(1)
        .mount(&server)
        .await;
    let filter = OrderFilter {
        min_ts: Some(CUTOFF_SECS + 100),
        ..Default::default()
    };
    let orders = client(&server, true)
        .orders(&filter, 100, 1_000)
        .await
        .expect("orders");
    assert_eq!(orders.len(), 2);
    let resting = &orders[0];
    assert_eq!(resting.order_id, "o-resting");
    assert_eq!(resting.outcome_side, OutcomeSide::Yes);
    assert_eq!(resting.book_side, BookSide::Bid);
    assert_eq!(resting.status, OrderStatus::Resting);
    // Hand-computed: the yes and no legs price a whole dollar.
    assert_eq!(
        resting.yes_price_dollars + resting.no_price_dollars,
        dec("1.00")
    );
    assert_eq!(
        resting.fill_count_fp + resting.remaining_count_fp,
        resting.initial_count_fp
    );
    assert_eq!(resting.subaccount_number, Some(0));
    let executed = &orders[1];
    assert_eq!(executed.order_type, furball::types::OrderType::Market);
    assert_eq!(
        executed.created_time,
        Some(Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap())
    );
    // Hand-computed: 40 taker fills at the 0.60 yes leg cost 24.00, 10 maker
    // fills at the 0.40 no leg cost 4.00.
    assert_eq!(executed.taker_fill_cost_dollars, dec("24.00"));
    assert_eq!(executed.maker_fill_cost_dollars, dec("4.00"));
    // Hand-computed fees from the schedule: 0.07 x 40 x 0.6 x 0.4 and 0.0175 x 10 x 0.6 x 0.4.
    assert_eq!(executed.taker_fees_dollars, dec("0.672"));
    assert_eq!(executed.maker_fees_dollars, dec("0.042"));
    assert_eq!(executed.order_group_id.as_deref(), Some("g1"));
    assert_eq!(executed.exchange_index, Some(1));
}

#[tokio::test]
async fn get_order_decodes_a_single_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders/o-123")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("order")))
        .expect(1)
        .mount(&server)
        .await;
    let order = client(&server, true)
        .get_order("o-123")
        .await
        .expect("order");
    assert_eq!(order.order_id, "o-123");
    assert_eq!(order.outcome_side, OutcomeSide::No);
    assert_eq!(order.book_side, BookSide::Ask);
    assert_eq!(
        order.yes_price_dollars + order.no_price_dollars,
        dec("1.00")
    );
    assert_eq!(
        order.fill_count_fp + order.remaining_count_fp,
        order.initial_count_fp
    );
}

#[tokio::test]
async fn get_order_without_credentials_fails_with_auth_error() {
    let server = MockServer::start().await;
    let error = client(&server, false)
        .get_order("o-123")
        .await
        .expect_err("no credentials");
    assert!(matches!(error, KalshiError::Auth(_)));
}

#[tokio::test]
async fn order_queue_position_decodes_fixed_point() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/orders/o-123/queue_position"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queue_position")))
        .expect(1)
        .mount(&server)
        .await;
    let position = client(&server, true)
        .order_queue_position("o-123")
        .await
        .expect("queue position");
    assert_eq!(position, dec("250.50"));
}

#[tokio::test]
async fn order_queue_positions_joins_market_tickers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/orders/queue_positions"
        )))
        .and(query_param(
            "market_tickers",
            "KXHIGHNY-26NOV07-T68,KXHIGHNY-26NOV07-T72",
        ))
        .and(query_param("subaccount", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queue_positions")))
        .expect(1)
        .mount(&server)
        .await;
    let positions = client(&server, true)
        .order_queue_positions(
            &["KXHIGHNY-26NOV07-T68", "KXHIGHNY-26NOV07-T72"],
            None,
            Some(2),
        )
        .await
        .expect("queue positions");
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].queue_position_fp, dec("250.50"));
    assert_eq!(positions[1].queue_position_fp, dec("0.00"));
}

#[tokio::test]
async fn fills_fixture_decodes_fees_from_the_schedule() {
    let server = MockServer::start().await;
    mount_cutoff(&server, 1).await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/fills")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("fills")))
        .expect(1)
        .mount(&server)
        .await;
    let filter = FillFilter {
        min_ts: Some(CUTOFF_SECS + 100),
        ..Default::default()
    };
    let merged = client(&server, true)
        .fills(&filter, 100, 1_000)
        .await
        .expect("fills");
    assert_eq!(merged.len(), 2);
    let taker = &merged[0];
    assert_eq!(taker.fill_id, "f1");
    assert_eq!(taker.order_id, "o1");
    assert!(taker.is_taker);
    // Hand-computed: 0.07 x 5 x 0.6 x 0.4.
    assert_eq!(taker.fee_cost, dec("0.084"));
    let maker = &merged[1];
    assert!(!maker.is_taker);
    // Hand-computed: 0.0175 x 40 x 0.5 x 0.5.
    assert_eq!(maker.fee_cost, dec("0.175"));
}

#[tokio::test]
async fn position_history_merges_live_and_archived_and_prefers_live_rows() {
    let server = MockServer::start().await;
    // Mounted first, so it wins over the general mock when the cursor is set.
    Mock::given(path(format!("{API_PREFIX}/portfolio/positions")))
        .and(query_param("cursor", "next"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "market_positions": [], "event_positions": [], "cursor": "" }),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/positions")))
        .and(query_param("settlement_status", "all"))
        .and(query_param("subaccount", "0"))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("positions")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/historical/positions")))
        .and(query_param("subaccount", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("historical_positions")))
        .expect(1)
        .mount(&server)
        .await;
    let positions = client(&server, true)
        .position_history(0, 100, 1_000)
        .await
        .expect("positions");
    let market_tickers: Vec<_> = positions
        .market_positions
        .iter()
        .map(|position| position.ticker.as_str())
        .collect();
    assert_eq!(
        market_tickers,
        [
            "KXHIGHNY-26NOV07-T68",
            "KXHIGHNY-26NOV07-T72",
            "KXHIGHNY-26NOV07-T88"
        ]
    );
    // The overlapping T68 row keeps its live values, not the stale archived ones.
    assert_eq!(positions.market_positions[0].position_fp, dec("150.00"));
    let event_tickers: Vec<_> = positions
        .event_positions
        .iter()
        .map(|position| position.event_ticker.as_str())
        .collect();
    assert_eq!(event_tickers, ["KXHIGHNY", "KXELONMARS"]);
    assert_eq!(
        positions.event_positions[0].total_cost_dollars,
        dec("340.00")
    );
}
