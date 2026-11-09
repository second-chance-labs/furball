mod common;

use chrono::{TimeZone, Utc};
use common::{API_PREFIX, client};
use furball::types::MarketResult;
use furball::{KalshiError, PositionFilter, SettlementFilter, SettlementStatus};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::time::{Duration, Instant};
use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("fixture json")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal")
}

#[tokio::test]
async fn balance_decodes_cents_and_fixed_point_dollars() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/balance")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("balance")))
        .expect(1)
        .mount(&server)
        .await;
    let balance = client(&server, true)
        .balance(None, None)
        .await
        .expect("balance");
    assert_eq!(balance.balance, 123_456_789);
    assert_eq!(balance.balance_dollars, dec("1234567.89"));
    assert_eq!(balance.portfolio_value, 987_654_321);
    assert_eq!(balance.updated_ts, 1_761_785_148);
    let breakdown = balance.balance_breakdown.expect("breakdown");
    let sum: Decimal = breakdown.iter().map(|entry| entry.balance).sum();
    assert_eq!(sum, balance.balance_dollars);
}

#[tokio::test]
async fn balance_subaccount_and_exchange_index_become_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/balance")))
        .and(query_param("subaccount", "2"))
        .and(query_param("exchange_index", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("balance")))
        .expect(1)
        .mount(&server)
        .await;
    client(&server, true)
        .balance(Some(2), Some(1))
        .await
        .expect("balance");
}

#[tokio::test]
async fn balance_without_credentials_fails_with_auth_error() {
    let server = MockServer::start().await;
    let error = client(&server, false)
        .balance(None, None)
        .await
        .expect_err("no credentials");
    assert!(matches!(error, KalshiError::Auth(_)));
}

#[tokio::test]
async fn balance_requests_wait_for_their_own_per_second_bucket() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/portfolio/balance")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("balance")))
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let started = Instant::now();
    client.balance(None, None).await.expect("first");
    client.balance(None, None).await.expect("second");
    // Basic tier: one balance request per second, no burst.
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn positions_decode_event_aggregates_that_sum_the_market_rows() {
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
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("positions")))
        .expect(1)
        .mount(&server)
        .await;
    let positions = client(&server, true)
        .positions(&PositionFilter::default(), 100, 1_000)
        .await
        .expect("positions");
    assert_eq!(positions.len(), 3);
    assert_eq!(positions.market_positions[0].position_fp, dec("150.00"));
    assert_eq!(positions.market_positions[1].position_fp, dec("-40.00"));
    let event = &positions.event_positions[0];
    // Hand-computed from the two market rows: 250.00 + 90.00, 150.00 + 40.00,
    // 75.00 + (-40.00), 12.50 + (-3.50), 1.23 + 0.47.
    assert_eq!(event.total_cost_dollars, dec("340.00"));
    assert_eq!(event.total_cost_shares_fp, dec("190.00"));
    assert_eq!(event.event_exposure_dollars, dec("35.00"));
    assert_eq!(event.realized_pnl_dollars, dec("9.00"));
    assert_eq!(event.fees_paid_dollars, dec("1.70"));
}

#[tokio::test]
async fn positions_follow_cursors_and_stop_at_max_items() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/portfolio/positions")))
        .respond_with(|request: &wiremock::Request| {
            let second = request
                .url
                .query_pairs()
                .any(|(key, value)| key == "cursor" && value == "next");
            let name = if second {
                "positions_page_2"
            } else {
                "positions"
            };
            ResponseTemplate::new(200).set_body_json(fixture(name))
        })
        .expect(2)
        .mount(&server)
        .await;
    let positions = client(&server, true)
        .positions(&PositionFilter::default(), 2, 4)
        .await
        .expect("positions");
    // Page order is [T68, T72, KXHIGHNY, T80, T76]; the cap keeps the first four.
    assert_eq!(positions.len(), 4);
    assert_eq!(positions.market_positions.len(), 3);
    assert_eq!(positions.event_positions.len(), 1);
    assert_eq!(positions.market_positions[2].ticker, "KXHIGHNY-26NOV07-T80");
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn positions_filter_maps_parameters_and_leaves_unset_status_to_the_server() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/portfolio/positions")))
        .respond_with(|request: &wiremock::Request| {
            let leaked = request
                .url
                .query_pairs()
                .any(|(key, _)| key == "settlement_status");
            ResponseTemplate::new(if leaked { 400 } else { 200 }).set_body_json(
                json!({ "market_positions": [], "event_positions": [], "cursor": "" }),
            )
        })
        .up_to_n_times(1)
        .mount(&server)
        .await;
    client(&server, true)
        .positions(&PositionFilter::default(), 10, 10)
        .await
        .expect("unsettled default");

    let filter = PositionFilter {
        settlement_status: Some(SettlementStatus::All),
        event_ticker: Some("KXHIGHNY".into()),
        subaccount: Some(3),
        ..Default::default()
    };
    Mock::given(path(format!("{API_PREFIX}/portfolio/positions")))
        .and(query_param("settlement_status", "all"))
        .and(query_param("event_ticker", "KXHIGHNY"))
        .and(query_param("subaccount", "3"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "market_positions": [], "event_positions": [], "cursor": "" }),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    client(&server, true)
        .positions(&filter, 10, 10)
        .await
        .expect("filtered");
}

#[tokio::test]
async fn settlements_decode_with_revenue_in_cents() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/settlements")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("settlements")))
        .expect(1)
        .mount(&server)
        .await;
    let settlements = client(&server, true)
        .settlements(&SettlementFilter::default(), 100, 1_000)
        .await
        .expect("settlements");
    assert_eq!(settlements.len(), 2);
    let yes = &settlements[0];
    assert_eq!(yes.market_result, MarketResult::Yes);
    assert_eq!(yes.yes_count_fp, dec("100.00"));
    assert_eq!(yes.yes_total_cost_dollars, dec("50.00"));
    // Hand-computed: 100 yes contracts pay out 100 cents each.
    assert_eq!(yes.revenue, 10_000);
    assert_eq!(yes.value, Some(100));
    assert_eq!(
        yes.settled_time,
        Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap()
    );
    let no = &settlements[1];
    assert_eq!(no.market_result, MarketResult::No);
    assert_eq!(no.no_count_fp, dec("100.00"));
    assert_eq!(no.value, None);
}

#[tokio::test]
async fn settlements_filter_maps_to_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/portfolio/settlements")))
        .and(query_param("ticker", "KXHIGHNY-26OCT31-T100"))
        .and(query_param("min_ts", "1761785148"))
        .and(query_param("subaccount", "3"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "settlements": [], "cursor": "" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = SettlementFilter {
        ticker: Some("KXHIGHNY-26OCT31-T100".into()),
        min_ts: Some(1_761_785_148),
        subaccount: Some(3),
        ..Default::default()
    };
    client(&server, true)
        .settlements(&filter, 10, 10)
        .await
        .expect("settlements");
}

#[tokio::test]
async fn subaccount_balances_decode_each_shard() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/subaccounts/balances")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("subaccount_balances")))
        .expect(1)
        .mount(&server)
        .await;
    let balances = client(&server, true)
        .subaccount_balances()
        .await
        .expect("balances");
    assert_eq!(balances.len(), 3);
    assert_eq!(balances[0].subaccount_number, 0);
    assert_eq!(balances[0].balance, dec("1234567.89"));
    assert_eq!(balances[1].subaccount_number, 1);
    assert_eq!(balances[1].balance, dec("42.00"));
    assert_eq!(balances[2].exchange_index, 1);
    assert_eq!(balances[2].balance, dec("0.00"));
}
