#![cfg(feature = "trading")]
#![allow(dead_code)]

mod common;

use std::str::FromStr;
use std::time::Duration;

use chrono::TimeZone;
use common::{API_PREFIX, KEY_ID, client, credentials, verify_rsa as verify};
use furball::{
    AcceptedSide, KalshiError, NewQuote, NewRfq, Quote, QuoteFilter, QuoteStatus, Rfq, RfqFilter,
    RfqSize, RfqStatus, TradingClient, WriteOutcome,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

const RFQ_ID: &str = "0191f2c4-9a3b-7d1e-8c5f-2a6b4e8d9f01";
const QUOTE_ID: &str = "0191f2c5-1b4c-8e2f-9d6a-3b7c5f9e0a12";

fn fixture(name: &str) -> Value {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("fixture json")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal")
}

fn trading_client(server: &MockServer) -> TradingClient {
    TradingClient::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials())
        .build()
        .expect("client")
}

fn new_rfq() -> NewRfq {
    NewRfq {
        market_ticker: "KXAPRIL-25FEB-YES".into(),
        size: RfqSize::Contracts(1000.into()),
        rest_remainder: true,
        replace_existing: false,
        obscure_creator_id: false,
        subtrader_id: None,
        subaccount: None,
    }
}

fn new_quote() -> NewQuote {
    NewQuote {
        rfq_id: RFQ_ID.into(),
        yes_bid: dec("0.5400"),
        no_bid: dec("0.4600"),
        rest_remainder: false,
        post_only: true,
        subaccount: Some(2),
    }
}

/// One RFQ fixture with a different id, for pagination tests.
fn rfq_with_id(id: &str) -> Value {
    let mut value = fixture("rfq");
    value["id"] = json!(id);
    value
}

/// One quote fixture with a different id, for pagination tests.
fn quote_with_id(id: &str) -> Value {
    let mut value = fixture("quote");
    value["id"] = json!(id);
    value
}

static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn new_rfq_serializes_exactly_one_size_mode() {
    let contracts = serde_json::to_value(new_rfq()).expect("serialize");
    assert_eq!(contracts["market_ticker"], "KXAPRIL-25FEB-YES");
    assert_eq!(contracts["contracts_fp"], "1000");
    assert_eq!(contracts["rest_remainder"], true);
    assert!(contracts.get("target_cost_dollars").is_none());
    assert!(contracts.get("target_cost_excludes_fees").is_none());
    assert!(contracts.get("replace_existing").is_none());
    assert!(contracts.get("obscure_creator_id").is_none());
    assert!(contracts.get("subtrader_id").is_none());
    assert!(contracts.get("subaccount").is_none());

    let mut target_cost = new_rfq();
    target_cost.size = RfqSize::TargetCost {
        dollars: dec("550.0000"),
        excludes_fees: true,
    };
    target_cost.replace_existing = true;
    target_cost.obscure_creator_id = true;
    target_cost.subtrader_id = Some("st-1".into());
    target_cost.subaccount = Some(3);
    let value = serde_json::to_value(&target_cost).expect("serialize");
    assert!(value.get("contracts_fp").is_none());
    assert_eq!(value["target_cost_dollars"], "550.0000");
    assert_eq!(value["target_cost_excludes_fees"], true);
    assert_eq!(value["replace_existing"], true);
    assert_eq!(value["obscure_creator_id"], true);
    assert_eq!(value["subtrader_id"], "st-1");
    assert_eq!(value["subaccount"], 3);
}

#[test]
fn new_quote_serializes_fixed_point_bids() {
    let value = serde_json::to_value(new_quote()).expect("serialize");
    assert_eq!(value["rfq_id"], RFQ_ID);
    assert_eq!(value["yes_bid"], "0.5400");
    assert_eq!(value["no_bid"], "0.4600");
    assert_eq!(value["rest_remainder"], false);
    assert_eq!(value["post_only"], true);
    assert_eq!(value["subaccount"], 2);

    let mut minimal = new_quote();
    minimal.post_only = false;
    minimal.subaccount = None;
    let value = serde_json::to_value(&minimal).expect("serialize");
    assert!(value.get("post_only").is_none());
    assert!(value.get("subaccount").is_none());
}

#[test]
fn statuses_round_trip_and_gate_transitions() {
    let open: QuoteStatus = serde_json::from_value(json!("open")).expect("open");
    let accepted: QuoteStatus = serde_json::from_value(json!("accepted")).expect("accepted");
    let confirmed: QuoteStatus = serde_json::from_value(json!("confirmed")).expect("confirmed");
    let executed: QuoteStatus = serde_json::from_value(json!("executed")).expect("executed");
    let cancelled: QuoteStatus = serde_json::from_value(json!("cancelled")).expect("cancelled");
    let unknown: QuoteStatus = serde_json::from_value(json!("weird")).expect("unknown");
    assert_eq!(unknown, QuoteStatus::Other("weird".into()));

    assert!(open.can_accept());
    assert!(!accepted.can_accept() && !confirmed.can_accept() && !executed.can_accept());
    assert!(accepted.can_confirm());
    assert!(!open.can_confirm() && !confirmed.can_confirm() && !cancelled.can_confirm());
    assert!(open.can_delete());
    assert!(!accepted.can_delete() && !cancelled.can_delete());

    assert_eq!(
        serde_json::to_value(AcceptedSide::Yes).expect("yes"),
        json!("yes")
    );
    assert_eq!(
        serde_json::to_value(AcceptedSide::No).expect("no"),
        json!("no")
    );

    let open: RfqStatus = serde_json::from_value(json!("open")).expect("open");
    let closed: RfqStatus = serde_json::from_value(json!("closed")).expect("closed");
    let zombie: RfqStatus = serde_json::from_value(json!("zombie")).expect("zombie");
    assert_eq!(open, RfqStatus::Open);
    assert_eq!(closed, RfqStatus::Closed);
    assert_eq!(zombie, RfqStatus::Other("zombie".into()));
}

#[test]
fn rfq_fixture_decodes_every_field() {
    let rfq: Rfq = serde_json::from_value(fixture("rfq")).expect("decode");
    assert_eq!(rfq.rfq_id, RFQ_ID);
    assert_eq!(rfq.creator_id, "comm-creator-abc");
    assert_eq!(rfq.market_ticker, "KXAPRIL-25FEB-YES");
    assert_eq!(rfq.contracts_fp, dec("1000.00"));
    assert_eq!(rfq.target_cost_dollars, Some(dec("550.0000")));
    assert!(!rfq.target_cost_excludes_fees);
    assert_eq!(rfq.status, RfqStatus::Open);
    assert_eq!(
        rfq.created_ts,
        chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 0, 0).unwrap()
    );
    assert_eq!(
        rfq.updated_ts,
        Some(chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 0, 5).unwrap())
    );
    assert!(rfq.rest_remainder);
    assert_eq!(rfq.creator_subaccount, Some(1));
    assert_eq!(rfq.creator_user_id, Some("user-creator-1".into()));
    assert_eq!(rfq.rfq_batch_id, Some("batch-900".into()));
    assert_eq!(rfq.mve_collection_ticker, Some("KMVE-25".into()));
    assert_eq!(rfq.mve_selected_legs.len(), 1);
    assert_eq!(rfq.mve_selected_legs[0].event_ticker, "KXAPRIL-25FEB");
    assert_eq!(
        rfq.mve_selected_legs[0].yes_settlement_value_dollars,
        Some(dec("0.5500"))
    );
}

#[test]
fn rfq_missing_optional_fields_defaults() {
    let minimal: Rfq = serde_json::from_value(json!({
        "id": "rfq-min",
        "creator_id": "c1",
        "market_ticker": "KX-1-YES",
        "contracts_fp": "10",
        "status": "closed",
        "created_ts": "2025-02-11T10:00:00Z"
    }))
    .expect("decode");
    assert_eq!(minimal.status, RfqStatus::Closed);
    assert_eq!(minimal.target_cost_dollars, None);
    assert!(minimal.updated_ts.is_none());
    assert!(!minimal.rest_remainder);
    assert!(minimal.mve_selected_legs.is_empty());
}

#[test]
fn quote_fixture_decodes_every_field() {
    let quote: Quote = serde_json::from_value(fixture("quote")).expect("decode");
    assert_eq!(quote.quote_id, QUOTE_ID);
    assert_eq!(quote.rfq_id, RFQ_ID);
    assert_eq!(quote.creator_id, "comm-maker-def");
    assert_eq!(quote.rfq_creator_id, "comm-creator-abc");
    assert_eq!(quote.market_ticker, "KXAPRIL-25FEB-YES");
    assert_eq!(quote.contracts_fp, dec("1000.00"));
    assert_eq!(quote.yes_bid_dollars, dec("0.5400"));
    assert_eq!(quote.no_bid_dollars, dec("0.4600"));
    assert_eq!(quote.status, QuoteStatus::Open);
    assert_eq!(
        quote.created_ts,
        chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 1, 0).unwrap()
    );
    assert_eq!(quote.accepted_side, None);
    assert!(quote.accepted_ts.is_none());
    assert!(!quote.rest_remainder);
    assert!(!quote.post_only);
    assert_eq!(quote.yes_contracts_fp, Some(dec("1000.00")));
    assert_eq!(quote.no_contracts_fp, Some(dec("1000.00")));
    assert_eq!(quote.rfq_target_cost_dollars, Some(dec("550.0000")));
    assert_eq!(quote.creator_subaccount, Some(0));
    assert_eq!(quote.rfq_creator_subaccount, Some(1));
    assert_eq!(quote.creator_order_id, Some("o-maker-1".into()));
    assert_eq!(quote.rfq_creator_order_id, Some("o-rfq-1".into()));
}

#[test]
fn quote_with_accepted_side_decodes() {
    let mut value = fixture("quote");
    value["status"] = json!("accepted");
    value["accepted_side"] = json!("no");
    value["accepted_ts"] = json!("2025-02-11T10:02:00Z");
    let quote: Quote = serde_json::from_value(value).expect("decode");
    assert_eq!(quote.status, QuoteStatus::Accepted);
    assert_eq!(quote.accepted_side, Some(AcceptedSide::No));
    assert_eq!(
        quote.accepted_ts,
        Some(chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 2, 0).unwrap())
    );
}

#[tokio::test]
async fn create_rfq_sends_a_signed_request_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let created = fixture("created_rfq");
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/rfqs")))
        .and(header_exists(furball::ACCESS_KEY_HEADER))
        .and(header_exists(furball::ACCESS_TIMESTAMP_HEADER))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["market_ticker"] == "KXAPRIL-25FEB-YES"
                && body["contracts_fp"] == "1000"
                && body["rest_remainder"] == true;
            let signed_ok = header(furball::ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!("{API_PREFIX}/communications/rfqs"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 })
                .set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.create_rfq(&new_rfq()).await.expect("created") {
        WriteOutcome::Sent(created) => assert_eq!(created.rfq_id, RFQ_ID),
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_rfq_5xx_surfaces_ambiguity() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/rfqs")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client.create_rfq(&new_rfq()).await.expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[tokio::test]
async fn get_rfq_decodes_the_fixture() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let valid = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "GET",
                &format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 200 } else { 401 })
                .set_body_json(json!({ "rfq": fixture("rfq") }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let rfq = client(&server, true).get_rfq(RFQ_ID).await.expect("rfq");
    assert_eq!(rfq.rfq_id, RFQ_ID);
    assert_eq!(rfq.status, RfqStatus::Open);
    assert_eq!(rfq.contracts_fp, dec("1000.00"));
}

#[tokio::test]
async fn get_rfq_missing_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let error = client(&server, true)
        .get_rfq(RFQ_ID)
        .await
        .expect_err("missing");
    assert!(matches!(error, KalshiError::NotFound), "{error:?}");
}

#[tokio::test]
async fn list_rfqs_follows_cursors_and_sends_the_filters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/rfqs")))
        .and(query_param_is_missing("cursor"))
        .and(query_param("market_ticker", "KXAPRIL-25FEB-YES"))
        .and(query_param("event_ticker", "KXAPRIL-25FEB"))
        .and(query_param("status", "open"))
        .and(query_param("subaccount", "1"))
        .and(query_param("user_filter", "self"))
        .and(query_param("limit", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "rfqs": [rfq_with_id("rfq-a")], "cursor": "c1" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/rfqs")))
        .and(query_param("cursor", "c1"))
        .and(query_param("market_ticker", "KXAPRIL-25FEB-YES"))
        .and(query_param("status", "open"))
        .and(query_param("user_filter", "self"))
        .and(query_param("limit", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "rfqs": [rfq_with_id("rfq-b")], "cursor": "" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = RfqFilter {
        market_ticker: Some("KXAPRIL-25FEB-YES".into()),
        event_ticker: Some("KXAPRIL-25FEB".into()),
        status: Some(RfqStatus::Open),
        subaccount: Some(1),
        created_by_me: true,
    };
    let rfqs = client(&server, true)
        .list_rfqs(&filter, 1, 100)
        .await
        .expect("rfqs");
    let ids: Vec<_> = rfqs.iter().map(|rfq| rfq.rfq_id.as_str()).collect();
    assert_eq!(ids, ["rfq-a", "rfq-b"]);
}

#[tokio::test]
async fn rfq_reads_require_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "rfqs": [], "cursor": "" })))
        .mount(&server)
        .await;
    let error = client(&server, false)
        .list_rfqs(&RfqFilter::default(), 10, 100)
        .await
        .expect_err("no credentials");
    assert!(matches!(error, KalshiError::Auth(_)), "{error:?}");
}

#[tokio::test]
async fn delete_rfq_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let valid = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "DELETE",
                &format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client.delete_rfq(RFQ_ID).await.expect("deleted") {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_quote_sends_a_signed_request_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let created = fixture("created_quote");
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/quotes")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["rfq_id"] == RFQ_ID
                && body["yes_bid"] == "0.5400"
                && body["no_bid"] == "0.4600"
                && body["rest_remainder"] == false
                && body["post_only"] == true
                && body["subaccount"] == 2;
            let signed_ok = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "POST",
                &format!("{API_PREFIX}/communications/quotes"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 })
                .set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.create_quote(&new_quote()).await.expect("created") {
        WriteOutcome::Sent(created) => assert_eq!(created.quote_id, QUOTE_ID),
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_quote_5xx_surfaces_ambiguity() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/quotes")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .create_quote(&new_quote())
        .await
        .expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[tokio::test]
async fn get_quote_decodes_the_fixture() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "quote": fixture("quote") })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let quote = client(&server, true)
        .get_quote(RFQ_ID, QUOTE_ID)
        .await
        .expect("quote");
    assert_eq!(quote.quote_id, QUOTE_ID);
    assert_eq!(quote.rfq_id, RFQ_ID);
    assert_eq!(quote.status, QuoteStatus::Open);
    assert_eq!(quote.yes_bid_dollars, dec("0.5400"));
}

#[tokio::test]
async fn list_quotes_follows_cursors_and_sends_the_filters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/quotes")))
        .and(query_param_is_missing("cursor"))
        .and(query_param("rfq_id", RFQ_ID))
        .and(query_param("status", "accepted"))
        .and(query_param("min_ts", "1739265600"))
        .and(query_param("max_ts", "1739269200"))
        .and(query_param("user_filter", "self"))
        .and(query_param("rfq_user_filter", "self"))
        .and(query_param("rfq_creator_subtrader_id", "st-1"))
        .and(query_param("limit", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "quotes": [quote_with_id("q-a")], "cursor": "c1" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/communications/quotes")))
        .and(query_param("cursor", "c1"))
        .and(query_param("rfq_id", RFQ_ID))
        .and(query_param("status", "accepted"))
        .and(query_param("user_filter", "self"))
        .and(query_param("rfq_user_filter", "self"))
        .and(query_param("limit", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "quotes": [quote_with_id("q-b")], "cursor": "" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let filter = QuoteFilter {
        rfq_id: Some(RFQ_ID.into()),
        status: Some(QuoteStatus::Accepted),
        min_ts: Some(1_739_265_600),
        max_ts: Some(1_739_269_200),
        created_by_me: true,
        on_own_rfq: true,
        rfq_creator_subtrader_id: Some("st-1".into()),
    };
    let quotes = client(&server, true)
        .list_quotes(&filter, 1, 100)
        .await
        .expect("quotes");
    let ids: Vec<_> = quotes.iter().map(|quote| quote.quote_id.as_str()).collect();
    assert_eq!(ids, ["q-a", "q-b"]);
}

#[tokio::test]
async fn delete_quote_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let valid = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "DELETE",
                &format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .delete_quote(RFQ_ID, QUOTE_ID)
        .await
        .expect("deleted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn accept_rfq_quote_sends_the_side_and_returns_unit() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/accept"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let valid = body["accepted_side"] == "yes"
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "PUT",
                    &format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/accept"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .accept_rfq_quote(RFQ_ID, QUOTE_ID, AcceptedSide::Yes)
        .await
        .expect("accepted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn accept_quote_hits_the_deprecated_quote_scoped_endpoint() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/quotes/{QUOTE_ID}/accept"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            ResponseTemplate::new(if body["accepted_side"] == "no" {
                204
            } else {
                401
            })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .accept_quote(QUOTE_ID, AcceptedSide::No)
        .await
        .expect("accepted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn confirm_rfq_quote_sends_a_signed_put_without_a_body() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/confirm"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .expect("header")
                    .to_str()
                    .expect("utf8")
                    .to_owned()
            };
            let valid = request.body.is_empty()
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "PUT",
                    &format!("{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/confirm"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .confirm_rfq_quote(RFQ_ID, QUOTE_ID)
        .await
        .expect("confirmed")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn confirm_quote_hits_the_deprecated_quote_scoped_endpoint() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/quotes/{QUOTE_ID}/confirm"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client.confirm_quote(QUOTE_ID).await.expect("confirmed") {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn idempotent_puts_surface_plain_errors_on_5xx() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/accept"
        )))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .accept_rfq_quote(RFQ_ID, QUOTE_ID, AcceptedSide::Yes)
        .await
        .expect_err("idempotent write");
    assert!(
        matches!(error, KalshiError::Api { status, .. } if status.as_u16() == 500),
        "{error:?}"
    );
}

#[tokio::test]
async fn dry_run_clients_make_zero_http_requests() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = TradingClient::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials())
        .dry_run(true)
        .build()
        .expect("client");
    assert!(client.is_dry_run());

    match client.create_rfq(&new_rfq()).await.expect("dry run") {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "POST");
            assert_eq!(dry.path, "/communications/rfqs");
            assert_eq!(dry.cost, 10);
            assert!(dry.timestamp_ms > 0);
            assert_eq!(dry.body["market_ticker"], "KXAPRIL-25FEB-YES");
            assert_eq!(dry.body["contracts_fp"], "1000");
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    match client
        .accept_rfq_quote(RFQ_ID, QUOTE_ID, AcceptedSide::No)
        .await
        .expect("dry run")
    {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "PUT");
            assert_eq!(
                dry.path,
                format!("/communications/rfqs/{RFQ_ID}/quotes/{QUOTE_ID}/accept")
            );
            assert_eq!(dry.body["accepted_side"], "no");
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    match client.delete_rfq(RFQ_ID).await.expect("dry run") {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "DELETE");
            assert_eq!(dry.body, Value::Null);
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    assert!(server.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn halted_clients_make_zero_http_requests() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = trading_client(&server);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("created_rfq")))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    TradingClient::halt();
    let error = client.create_rfq(&new_rfq()).await.expect_err("halted");
    assert!(matches!(error, KalshiError::TradingHalted));
    let error = client
        .accept_rfq_quote(RFQ_ID, QUOTE_ID, AcceptedSide::Yes)
        .await
        .expect_err("halted");
    assert!(matches!(error, KalshiError::TradingHalted));
    let error = client
        .confirm_rfq_quote(RFQ_ID, QUOTE_ID)
        .await
        .expect_err("halted");
    assert!(matches!(error, KalshiError::TradingHalted));
    assert!(server.received_requests().await.expect("log").is_empty());

    TradingClient::resume();
    let outcome = client.create_rfq(&new_rfq()).await.expect("resumed");
    assert!(matches!(outcome, WriteOutcome::Sent(_)));
    assert_eq!(server.received_requests().await.expect("log").len(), 1);
}
