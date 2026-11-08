mod common;

use chrono::{TimeZone, Utc};
use common::{API_PREFIX, client};
use furball::types::{FeeType, OutcomeSide};
use furball::{KalshiError, MarketFilter, MarketStatus, Period};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("fixture json")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal")
}

async fn serve(server: &MockServer, route: &str, fixture_name: &str) {
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}{route}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(fixture_name)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn series_and_fee_changes_decode_with_exact_decimals() {
    let server = MockServer::start().await;
    serve(&server, "/series/KXHIGHNY", "series").await;
    serve(&server, "/series/fee_changes", "series_fee_changes").await;
    let client = client(&server, false);

    let series = client.series("KXHIGHNY").await.expect("series");
    assert_eq!(series.fee_type, FeeType::Quadratic);
    assert_eq!(series.fee_multiplier, dec("1"));
    assert_eq!(series.volume_fp, None);

    let changes = client
        .series_fee_changes(Some("KXRTX5090W"), true)
        .await
        .expect("changes");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].fee_type, FeeType::QuadraticWithMakerFees);
    assert_eq!(
        changes[0].scheduled_ts,
        Utc.with_ymd_and_hms(2026, 9, 3, 18, 35, 23).unwrap()
    );
}

#[tokio::test]
async fn series_fee_changes_request_asks_for_history() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/series/fee_changes")))
        .and(query_param("series_ticker", "S"))
        .and(query_param("show_historical", "true"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "series_fee_change_arr": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    client(&server, false)
        .series_fee_changes(Some("S"), true)
        .await
        .expect("ok");
}

#[test]
fn fee_multipliers_decode_from_json_numbers_without_float_error() {
    let change: furball::types::EventFeeChange = serde_json::from_value(json!({
        "id": "1", "event_ticker": "E", "series_ticker": "S", "fee_type_override": null,
        "fee_multiplier_override": 0.0175, "scheduled_ts": "2026-01-01T00:00:00Z",
    }))
    .expect("decode");
    assert_eq!(change.fee_multiplier_override, Some(dec("0.0175")));
}

#[test]
fn unknown_fee_types_are_preserved_not_rejected() {
    let parsed: FeeType = serde_json::from_value(json!("brand_new")).expect("decode");
    assert_eq!(parsed, FeeType::Other("brand_new".into()));
    assert_eq!(
        serde_json::to_value(&parsed).expect("encode"),
        json!("brand_new")
    );
}

#[tokio::test]
async fn event_fee_changes_treat_null_overrides_as_cleared() {
    let server = MockServer::start().await;
    serve(&server, "/events/fee_changes", "event_fee_changes").await;
    let changes = client(&server, false)
        .event_fee_changes(None, 100, 1000)
        .await
        .expect("changes");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].fee_type_override, Some(FeeType::Flat));
    assert_eq!(changes[0].fee_multiplier_override, Some(dec("0.5")));
    assert_eq!(changes[1].fee_type_override, None);
    assert_eq!(changes[1].fee_multiplier_override, None);
}

#[tokio::test]
async fn events_decode_with_nested_markets_and_single_event_lookup() {
    let server = MockServer::start().await;
    serve(&server, "/events", "events").await;
    let events = client(&server, false)
        .events(true, 1, 10)
        .await
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].series_ticker, "KXELONMARS");
    assert_eq!(events[0].markets.as_ref().expect("nested").len(), 1);

    let event_body = json!({ "event": fixture("events")["events"][0], "markets": fixture("events")["events"][0]["markets"] });
    Mock::given(path(format!("{API_PREFIX}/events/KXELONMARS-99")))
        .respond_with(ResponseTemplate::new(200).set_body_json(event_body))
        .mount(&server)
        .await;
    let event = client(&server, false)
        .event("KXELONMARS-99")
        .await
        .expect("event");
    assert_eq!(event.markets[0].ticker, "KXELONMARS-99");
}

#[tokio::test]
async fn markets_live_and_historical_decode_settlement_fields() {
    let server = MockServer::start().await;
    serve(&server, "/markets", "markets").await;
    serve(&server, "/historical/markets", "historical_markets").await;
    let client = client(&server, false);

    let filter = MarketFilter {
        status: Some(MarketStatus::Settled),
        ..MarketFilter::series("KXHIGHNY")
    };
    let live = client.markets(&filter, 2, 10).await.expect("live");
    assert_eq!(live[0].ticker, "KXHIGHNY-26NOV07-T68");
    assert_eq!(live[0].result, "no");
    assert_eq!(live[0].settlement_value_dollars, Some(dec("0.0000")));
    assert!(live[0].settlement_ts.is_some());

    let archived = client
        .historical_markets(&MarketFilter::series("KXHIGHNY"), 2, 10)
        .await
        .expect("historical");
    assert_eq!(archived[0].ticker, "KXHIGHNY-26SEP07-T84");
}

#[tokio::test]
async fn market_filter_maps_to_query_parameters_and_historical_ignores_status() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/markets")))
        .and(query_param("status", "settled"))
        .and(query_param("series_ticker", "S"))
        .and(query_param("min_settled_ts", "5"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "markets": [], "cursor": "" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(format!("{API_PREFIX}/historical/markets")))
        .and(query_param("series_ticker", "S"))
        .respond_with(|request: &wiremock::Request| {
            let leaked = request
                .url
                .query_pairs()
                .any(|(key, _)| key == "status" || key == "min_settled_ts");
            ResponseTemplate::new(if leaked { 400 } else { 200 })
                .set_body_json(json!({ "markets": [], "cursor": "" }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, false);
    let filter = MarketFilter {
        status: Some(MarketStatus::Settled),
        min_settled_ts: Some(5),
        ..MarketFilter::series("S")
    };
    client.markets(&filter, 10, 10).await.expect("live");
    client
        .historical_markets(&filter, 10, 10)
        .await
        .expect("historical");
}

#[tokio::test]
async fn candlesticks_decode_live_and_historical_field_names_into_one_type() {
    let server = MockServer::start().await;
    serve(&server, "/markets/KXHIGHNY-26NOV07-T68", "markets_one_live").await;
    serve(
        &server,
        "/series/KXHIGHNY/markets/KXHIGHNY-26NOV07-T68/candlesticks",
        "candlesticks",
    )
    .await;
    serve(&server, "/historical/cutoff", "cutoff").await;
    serve(
        &server,
        "/markets/KXHIGHNY-26SEP07-T84",
        "markets_one_historical",
    )
    .await;
    serve(
        &server,
        "/historical/markets/KXHIGHNY-26SEP07-T84/candlesticks",
        "historical_candlesticks",
    )
    .await;
    let client = client(&server, false);

    let live = client
        .get_candlesticks("KXHIGHNY", "KXHIGHNY-26NOV07-T68", 0, 1, Period::Hour)
        .await
        .expect("live");
    let archived = client
        .get_candlesticks("KXHIGHNY", "KXHIGHNY-26SEP07-T84", 0, 1, Period::Hour)
        .await
        .expect("historical");

    assert_eq!(live[0].end_period_ts, 1_794_049_200);
    assert_eq!(live[0].open_interest_fp, dec("1866.19"));
    assert_eq!(live[0].yes_ask.close_dollars, dec("0.0100"));
    assert_eq!(live[0].price.previous_dollars, Some(dec("0.0100")));
    assert_eq!(live[0].price.close_dollars, None);

    assert_eq!(archived[0].open_interest_fp, dec("1172.02"));
    assert_eq!(archived[0].yes_ask.close_dollars, dec("0.0100"));
    assert_eq!(archived[0].price.previous_dollars, Some(dec("0.0100")));
    assert_eq!(archived[0].price.open_dollars, None);
}

#[tokio::test]
async fn batch_candlesticks_split_into_groups_of_one_hundred_tickers() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/markets/candlesticks")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("batch_candlesticks")))
        .expect(2)
        .mount(&server)
        .await;
    let tickers: Vec<String> = (0..101).map(|index| format!("T{index}")).collect();
    let refs: Vec<&str> = tickers.iter().map(String::as_str).collect();
    let markets = client(&server, false)
        .batch_candlesticks(&refs, 0, 1, Period::Day, false)
        .await
        .expect("batch");
    assert_eq!(markets.len(), 2);
    assert_eq!(markets[0].market_ticker, "KXHIGHNY-26NOV07-T68");
    let requests = server.received_requests().await.expect("recorded");
    let first = requests[0]
        .url
        .query_pairs()
        .find(|(key, _)| key == "market_tickers")
        .expect("param")
        .1
        .to_string();
    assert_eq!(first.split(',').count(), 100);
    assert!(
        requests[0]
            .url
            .query()
            .expect("query")
            .contains("period_interval=1440")
    );
}

#[tokio::test]
async fn trades_decode_from_both_endpoints_and_orderbook_levels_keep_exact_values() {
    let server = MockServer::start().await;
    serve(&server, "/historical/cutoff", "cutoff").await;
    serve(&server, "/markets/trades", "trades").await;
    serve(&server, "/historical/trades", "historical_trades").await;
    serve(
        &server,
        "/markets/KXHIGHNY-26NOV09-T64/orderbook",
        "orderbook",
    )
    .await;
    let client = client(&server, false);

    let cutoff = client
        .cutoff()
        .await
        .expect("cutoff")
        .trades_created_ts
        .timestamp();
    let trades = client
        .get_trades(None, cutoff - 10, cutoff + 10, 100, 100)
        .await
        .expect("trades");
    assert_eq!(trades.len(), 6);
    assert_eq!(trades[0].taker_outcome_side, OutcomeSide::Yes);
    assert_eq!(
        trades[0].yes_price_dollars + trades[0].no_price_dollars,
        dec("1")
    );

    let book = client
        .orderbook("KXHIGHNY-26NOV09-T64", 3)
        .await
        .expect("book");
    assert!(book.yes_dollars.is_empty());
    assert_eq!(book.no_dollars[2].price_dollars, dec("0.9900"));
    assert_eq!(book.no_dollars[2].count_fp, dec("1462.00"));
}

#[tokio::test]
async fn cf_benchmarks_require_credentials_and_sign_the_request() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/cfbenchmarks/values")))
        .and(query_param("id", "BRTI"))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("cfbenchmarks_values")))
        .expect(1)
        .mount(&server)
        .await;
    let response = client(&server, true)
        .cf_benchmarks_values(&[("id", "BRTI")])
        .await
        .expect("values");
    assert_eq!(response.data["serverTime"], "2019-08-13T23:30:53.992Z");

    let error = client(&server, false)
        .cf_benchmarks_history_values(&[("id", "BRTI")])
        .await
        .expect_err("no credentials");
    assert!(matches!(error, KalshiError::Auth(_)));
}
