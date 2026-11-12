#![cfg(feature = "trading")]
#![allow(dead_code)]

mod common;

use std::str::FromStr;
use std::time::Duration;

use chrono::TimeZone;
use common::{API_PREFIX, KEY_ID, client, credentials, verify_rsa as verify};
use furball::types::OutcomeSide;
use furball::{
    AcceptBlockTradeProposal, AcceptRfqBatch, AcceptedRfqBatch, AcceptedSide, BlockTradeProposal,
    BlockTradeProposalFilter, BlockTradeProposalStatus, CreatedBlockTradeProposal,
    CreatedRfqBatchQuote, KalshiError, NewQuote, NewRfq, NewRfqBatch, NewRfqBatchQuote,
    ProposeBlockTrade, Quote, QuoteFilter, QuoteStatus, Rfq, RfqBatch, RfqBatchAcceptLimit,
    RfqBatchChild, RfqBatchQuoteEntry, RfqBatchSize, RfqBatchStatus, RfqChildSize, RfqFilter,
    RfqSize, RfqStatus, TickerPair, TradingClient, WriteOutcome,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

const RFQ_ID: &str = "0191f2c4-9a3b-7d1e-8c5f-2a6b4e8d9f01";
const QUOTE_ID: &str = "0191f2c5-1b4c-8e2f-9d6a-3b7c5f9e0a12";
const RFQ_BATCH_ID: &str = "0191f2c6-2c5d-8f3a-9e7b-4c8d6a0f1b23";
const RFQ_ID_2: &str = "0191f2c7-3d6e-9a4b-8f8c-5d9e7b1a2c34";
const BATCH_QUOTE_ID: &str = "0191f2c9-5f8a-bc6d-aab0-7fb09d3c4e56";
const PROPOSAL_ID: &str = "btp-1000";

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

fn batch_child(ticker: &str) -> RfqBatchChild {
    RfqBatchChild {
        market_ticker: Some(ticker.into()),
        selected_markets: None,
        size: None,
    }
}

fn new_rfq_batch() -> NewRfqBatch {
    NewRfqBatch {
        children: vec![
            batch_child("KXAPRIL-25FEB-YES"),
            batch_child("KXAPRIL-25MAY-NO"),
        ],
        mve_collection_ticker: None,
        size: Some(RfqBatchSize::TargetCost {
            dollars: dec("550.0000"),
            excludes_fees: true,
        }),
        replace_existing: false,
        obscure_creator_id: false,
        client_batch_id: Some("cb-777".into()),
        subtrader_id: None,
        subaccount: None,
    }
}

fn new_rfq_batch_quote() -> NewRfqBatchQuote {
    NewRfqBatchQuote {
        quotes: vec![
            RfqBatchQuoteEntry {
                rfq_id: RFQ_ID.into(),
                yes_bid: dec("0.5400"),
                no_bid: dec("0.4600"),
            },
            RfqBatchQuoteEntry {
                rfq_id: RFQ_ID_2.into(),
                yes_bid: dec("0.5100"),
                no_bid: dec("0.4900"),
            },
        ],
        rest_remainder: false,
        post_only: false,
        subaccount: Some(2),
    }
}

fn accept_rfq_batch() -> AcceptRfqBatch {
    AcceptRfqBatch {
        accepted_side: AcceptedSide::Yes,
        min_bids: vec![
            RfqBatchAcceptLimit {
                rfq_id: RFQ_ID.into(),
                bid_dollars: dec("0.5400"),
            },
            RfqBatchAcceptLimit {
                rfq_id: RFQ_ID_2.into(),
                bid_dollars: dec("0.5100"),
            },
        ],
        client_order_id: Some("co-1".into()),
    }
}

fn new_block_trade_proposal() -> ProposeBlockTrade {
    ProposeBlockTrade {
        buyer_user_id: "user-buyer-1".into(),
        buyer_subtrader_id: None,
        buyer_subaccount: Some(2),
        seller_user_id: "user-seller-1".into(),
        seller_subtrader_id: None,
        seller_subaccount: None,
        market_ticker: "KXAPRIL-25FEB-YES".into(),
        price_centi_cents: 5400,
        centicount: 100000,
        maker_side: OutcomeSide::Yes,
        expiration_ts: chrono::Utc.with_ymd_and_hms(2025, 2, 11, 12, 0, 0).unwrap(),
    }
}

/// One block trade proposal fixture with a different id, for pagination tests.
fn block_trade_proposal_with_id(id: &str) -> Value {
    let mut value = fixture("block_trade_proposal");
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

#[test]
fn new_rfq_batch_serializes_one_size_mode_and_children() {
    let value = serde_json::to_value(new_rfq_batch()).expect("serialize");
    assert_eq!(value["children"].as_array().expect("children").len(), 2);
    assert_eq!(value["children"][0]["market_ticker"], "KXAPRIL-25FEB-YES");
    assert_eq!(value["children"][1]["market_ticker"], "KXAPRIL-25MAY-NO");
    assert_eq!(value["target_cost_dollars"], "550.0000");
    assert_eq!(value["target_cost_excludes_fees"], true);
    assert_eq!(value["rest_remainder"], false);
    assert_eq!(value["client_batch_id"], "cb-777");
    assert!(value.get("contracts_fp").is_none());
    assert!(value.get("replace_existing").is_none());
    assert!(value.get("obscure_creator_id").is_none());
    assert!(value.get("mve_collection_ticker").is_none());
    assert!(value.get("subtrader_id").is_none());
    assert!(value.get("subaccount").is_none());

    let mut contracts = new_rfq_batch();
    contracts.size = Some(RfqBatchSize::Contracts(dec("1000.00")));
    contracts.client_batch_id = None;
    contracts.replace_existing = true;
    let value = serde_json::to_value(&contracts).expect("serialize");
    assert_eq!(value["contracts_fp"], "1000.00");
    assert!(value.get("target_cost_dollars").is_none());
    assert!(value.get("target_cost_excludes_fees").is_none());
    assert_eq!(value["rest_remainder"], false);
    assert!(value.get("client_batch_id").is_none());
    assert_eq!(value["replace_existing"], true);

    let mut per_child = new_rfq_batch();
    per_child.size = None;
    per_child.mve_collection_ticker = Some("KMVE-25".into());
    per_child.children = vec![
        RfqBatchChild {
            market_ticker: Some("KXAPRIL-25FEB-YES".into()),
            selected_markets: None,
            size: Some(RfqChildSize::Contracts(dec("250.00"))),
        },
        RfqBatchChild {
            market_ticker: None,
            selected_markets: Some(vec![TickerPair {
                market_ticker: "KX-1-YES".into(),
                event_ticker: "KX-1".into(),
                side: OutcomeSide::Yes,
            }]),
            size: Some(RfqChildSize::TargetCost(dec("90.00"))),
        },
    ];
    let value = serde_json::to_value(&per_child).expect("serialize");
    assert_eq!(value["mve_collection_ticker"], "KMVE-25");
    assert!(value.get("contracts_fp").is_none());
    assert!(value.get("target_cost_dollars").is_none());
    assert_eq!(value["children"][0]["contracts_fp"], "250.00");
    assert!(value["children"][0].get("target_cost_dollars").is_none());
    assert!(value["children"][0].get("selected_markets").is_none());
    assert!(value["children"][1].get("market_ticker").is_none());
    assert_eq!(
        value["children"][1]["selected_markets"][0]["market_ticker"],
        "KX-1-YES"
    );
    assert_eq!(
        value["children"][1]["selected_markets"][0]["event_ticker"],
        "KX-1"
    );
    assert_eq!(value["children"][1]["selected_markets"][0]["side"], "yes");
    assert_eq!(value["children"][1]["target_cost_dollars"], "90.00");
}

#[test]
fn new_rfq_batch_quote_serializes_every_entry() {
    let value = serde_json::to_value(new_rfq_batch_quote()).expect("serialize");
    assert_eq!(value["quotes"].as_array().expect("quotes").len(), 2);
    assert_eq!(value["quotes"][0]["rfq_id"], RFQ_ID);
    assert_eq!(value["quotes"][0]["yes_bid"], "0.5400");
    assert_eq!(value["quotes"][0]["no_bid"], "0.4600");
    assert_eq!(value["quotes"][1]["rfq_id"], RFQ_ID_2);
    assert_eq!(value["rest_remainder"], false);
    assert_eq!(value["subaccount"], 2);
    assert!(value.get("post_only").is_none());

    let mut post_only = new_rfq_batch_quote();
    post_only.post_only = true;
    post_only.subaccount = None;
    post_only.rest_remainder = true;
    let value = serde_json::to_value(&post_only).expect("serialize");
    assert_eq!(value["post_only"], true);
    assert!(value.get("subaccount").is_none());
    assert_eq!(value["rest_remainder"], true);
}

#[test]
fn accept_rfq_batch_serializes_min_bids() {
    let value = serde_json::to_value(accept_rfq_batch()).expect("serialize");
    assert_eq!(value["accepted_side"], "yes");
    assert_eq!(value["min_bids"].as_array().expect("min_bids").len(), 2);
    assert_eq!(value["min_bids"][0]["rfq_id"], RFQ_ID);
    assert_eq!(value["min_bids"][0]["bid_dollars"], "0.5400");
    assert_eq!(value["min_bids"][1]["rfq_id"], RFQ_ID_2);
    assert_eq!(value["min_bids"][1]["bid_dollars"], "0.5100");
    assert_eq!(value["client_order_id"], "co-1");

    let mut no_key = accept_rfq_batch();
    no_key.client_order_id = None;
    let value = serde_json::to_value(&no_key).expect("serialize");
    assert!(value.get("client_order_id").is_none());
}

#[test]
fn rfq_batch_status_round_trips() {
    let open: RfqBatchStatus = serde_json::from_value(json!("open")).expect("open");
    let accepted: RfqBatchStatus = serde_json::from_value(json!("accepted")).expect("accepted");
    let closed: RfqBatchStatus = serde_json::from_value(json!("closed")).expect("closed");
    let unknown: RfqBatchStatus = serde_json::from_value(json!("partly")).expect("unknown");
    assert_eq!(open, RfqBatchStatus::Open);
    assert_eq!(accepted, RfqBatchStatus::Accepted);
    assert_eq!(closed, RfqBatchStatus::Closed);
    assert_eq!(unknown, RfqBatchStatus::Other("partly".into()));
}

#[test]
fn rfq_batch_fixture_decodes_every_field() {
    let batch_value = fixture("created_rfq_batch")["rfq_batch"].clone();
    let batch: RfqBatch = serde_json::from_value(batch_value).expect("decode");
    assert_eq!(batch.rfq_batch_id, RFQ_BATCH_ID);
    assert_eq!(batch.creator_id, "comm-creator-abc");
    assert_eq!(batch.status, RfqBatchStatus::Open);
    assert_eq!(
        batch.created_ts,
        chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 0, 0).unwrap()
    );
    assert_eq!(batch.rfqs.len(), 2);
    assert_eq!(batch.rfqs[0].rfq_id, RFQ_ID);
    assert_eq!(batch.rfqs[1].rfq_id, RFQ_ID_2);
    assert_eq!(batch.rfqs[0].rfq_batch_id, Some(RFQ_BATCH_ID.into()));
    assert_eq!(
        batch.updated_ts,
        Some(chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 0, 5).unwrap())
    );
    assert_eq!(batch.client_batch_id, Some("cb-777".into()));
    assert_eq!(batch.creator_user_id, Some("user-creator-1".into()));
    assert_eq!(batch.creator_subaccount, Some(1));
    assert!(batch.accepted_ts.is_none());
    assert!(batch.closed_ts.is_none());
    assert!(batch.close_reason.is_none());
}

#[test]
fn accepted_rfq_batch_fixture_decodes() {
    let accepted: AcceptedRfqBatch =
        serde_json::from_value(fixture("accepted_rfq_batch")).expect("decode");
    assert_eq!(accepted.accepted_side, AcceptedSide::Yes);
    assert_eq!(accepted.quotes.len(), 2);
    assert_eq!(accepted.quotes[0].quote_id, QUOTE_ID);
    assert_eq!(accepted.quotes[0].rfq_id, RFQ_ID);
    assert_eq!(accepted.quotes[0].status, QuoteStatus::Accepted);
    assert_eq!(accepted.quotes[0].accepted_side, Some(AcceptedSide::Yes));
    assert_eq!(accepted.quotes[1].rfq_id, RFQ_ID_2);
    assert_eq!(accepted.quotes[1].yes_bid_dollars, dec("0.5100"));
}

#[test]
fn created_rfq_batch_quote_fixture_decodes() {
    let created: CreatedRfqBatchQuote =
        serde_json::from_value(fixture("created_rfq_batch_quote")).expect("decode");
    assert_eq!(created.batch_quote_id, BATCH_QUOTE_ID);
}

#[tokio::test]
async fn create_rfq_batch_sends_a_signed_request_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let created = fixture("created_rfq_batch");
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/rfq_batches")))
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
            let body_ok = body["rest_remainder"] == false
                && body["target_cost_dollars"] == "550.0000"
                && body["target_cost_excludes_fees"] == true
                && body["client_batch_id"] == "cb-777"
                && body["children"].as_array().expect("children").len() == 2
                && body["children"][0]["market_ticker"] == "KXAPRIL-25FEB-YES";
            let signed_ok = header(furball::ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!("{API_PREFIX}/communications/rfq_batches"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 })
                .set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .create_rfq_batch(&new_rfq_batch())
        .await
        .expect("created")
    {
        WriteOutcome::Sent(batch) => {
            assert_eq!(batch.rfq_batch_id, RFQ_BATCH_ID);
            assert_eq!(batch.rfqs.len(), 2);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_rfq_batch_without_client_batch_id_5xx_is_ambiguous() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/communications/rfq_batches")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let mut batch = new_rfq_batch();
    batch.client_batch_id = None;
    let error = client
        .create_rfq_batch(&batch)
        .await
        .expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[tokio::test]
async fn delete_rfq_batch_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}"
        )))
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
                &format!("{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .delete_rfq_batch(RFQ_BATCH_ID)
        .await
        .expect("deleted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn accept_rfq_batch_sends_min_bids_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let accepted = fixture("accepted_rfq_batch");
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/accept"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
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
            let body_ok = body["accepted_side"] == "yes"
                && body["min_bids"].as_array().expect("min_bids").len() == 2
                && body["min_bids"][0]["bid_dollars"] == "0.5400"
                && body["client_order_id"] == "co-1";
            let signed_ok = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "PUT",
                &format!("{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/accept"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if body_ok && signed_ok { 200 } else { 401 })
                .set_body_json(accepted.clone())
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .accept_rfq_batch(RFQ_BATCH_ID, &accept_rfq_batch())
        .await
        .expect("accepted")
    {
        WriteOutcome::Sent(accepted) => {
            assert_eq!(accepted.accepted_side, AcceptedSide::Yes);
            assert_eq!(accepted.quotes.len(), 2);
            assert_eq!(accepted.quotes[0].quote_id, QUOTE_ID);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_rfq_batch_quote_sends_every_entry_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let created = fixture("created_rfq_batch_quote");
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
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
            let body_ok = body["quotes"].as_array().expect("quotes").len() == 2
                && body["quotes"][0]["rfq_id"] == RFQ_ID
                && body["quotes"][0]["yes_bid"] == "0.5400"
                && body["rest_remainder"] == false
                && body["subaccount"] == 2;
            let signed_ok = verify(
                &header(furball::ACCESS_TIMESTAMP_HEADER),
                "POST",
                &format!("{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 })
                .set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .create_rfq_batch_quote(RFQ_BATCH_ID, &new_rfq_batch_quote())
        .await
        .expect("created")
    {
        WriteOutcome::Sent(created) => assert_eq!(created.batch_quote_id, BATCH_QUOTE_ID),
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_rfq_batch_quote_5xx_is_a_plain_error_not_ambiguity() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes"
        )))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .create_rfq_batch_quote(RFQ_BATCH_ID, &new_rfq_batch_quote())
        .await
        .expect_err("idempotent write");
    assert!(
        matches!(error, KalshiError::Api { status, .. } if status.as_u16() == 500),
        "{error:?}"
    );
}

#[tokio::test]
async fn delete_rfq_batch_quote_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes/{BATCH_QUOTE_ID}"
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
                &format!(
                    "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes/{BATCH_QUOTE_ID}"
                ),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .delete_rfq_batch_quote(RFQ_BATCH_ID, BATCH_QUOTE_ID)
        .await
        .expect("deleted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn confirm_rfq_batch_quote_sends_a_signed_put_without_a_body() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes/{BATCH_QUOTE_ID}/confirm"
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
                    &format!(
                        "{API_PREFIX}/communications/rfq_batches/{RFQ_BATCH_ID}/quotes/{BATCH_QUOTE_ID}/confirm"
                    ),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .confirm_rfq_batch_quote(RFQ_BATCH_ID, BATCH_QUOTE_ID)
        .await
        .expect("confirmed")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn dry_run_batch_writes_bill_each_child() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = TradingClient::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials())
        .dry_run(true)
        .build()
        .expect("client");

    match client
        .create_rfq_batch(&new_rfq_batch())
        .await
        .expect("dry run")
    {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "POST");
            assert_eq!(dry.path, "/communications/rfq_batches");
            assert_eq!(dry.cost, 20);
            assert_eq!(dry.body["rest_remainder"], false);
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    match client
        .create_rfq_batch_quote(RFQ_BATCH_ID, &new_rfq_batch_quote())
        .await
        .expect("dry run")
    {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "POST");
            assert_eq!(
                dry.path,
                format!("/communications/rfq_batches/{RFQ_BATCH_ID}/quotes")
            );
            assert_eq!(dry.cost, 20);
            assert_eq!(dry.body["quotes"].as_array().expect("quotes").len(), 2);
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    assert!(server.received_requests().await.expect("log").is_empty());
}

#[test]
fn propose_block_trade_serializes_required_and_optional_fields() {
    let value = serde_json::to_value(new_block_trade_proposal()).expect("serialize");
    assert_eq!(value["buyer_user_id"], "user-buyer-1");
    assert_eq!(value["buyer_subaccount"], 2);
    assert!(value.get("buyer_subtrader_id").is_none());
    assert_eq!(value["seller_user_id"], "user-seller-1");
    assert!(value.get("seller_subtrader_id").is_none());
    assert!(value.get("seller_subaccount").is_none());
    assert_eq!(value["market_ticker"], "KXAPRIL-25FEB-YES");
    assert_eq!(value["price_centi_cents"], 5400);
    assert_eq!(value["centicount"], 100000);
    assert_eq!(value["maker_side"], "yes");
    assert_eq!(value["expiration_ts"], "2025-02-11T12:00:00Z");

    let mut with_subtraders = new_block_trade_proposal();
    with_subtraders.buyer_subaccount = None;
    with_subtraders.buyer_subtrader_id = Some("st-buyer-1".into());
    with_subtraders.seller_subaccount = Some(3);
    with_subtraders.maker_side = OutcomeSide::No;
    let value = serde_json::to_value(&with_subtraders).expect("serialize");
    assert_eq!(value["buyer_subtrader_id"], "st-buyer-1");
    assert!(value.get("buyer_subaccount").is_none());
    assert_eq!(value["seller_subaccount"], 3);
    assert_eq!(value["maker_side"], "no");
}

#[test]
fn accept_block_trade_proposal_serializes_the_booking() {
    let none: AcceptBlockTradeProposal = AcceptBlockTradeProposal::default();
    assert_eq!(serde_json::to_value(&none).expect("serialize"), json!({}));

    let subaccount = AcceptBlockTradeProposal {
        subtrader_id: None,
        subaccount: Some(4),
    };
    let value = serde_json::to_value(&subaccount).expect("serialize");
    assert_eq!(value["subaccount"], 4);
    assert!(value.get("subtrader_id").is_none());

    let subtrader = AcceptBlockTradeProposal {
        subtrader_id: Some("st-1".into()),
        subaccount: None,
    };
    let value = serde_json::to_value(&subtrader).expect("serialize");
    assert_eq!(value["subtrader_id"], "st-1");
    assert!(value.get("subaccount").is_none());
}

#[test]
fn block_trade_proposal_status_round_trips() {
    let pending: BlockTradeProposalStatus = serde_json::from_value(json!("pending")).expect("p");
    let executed: BlockTradeProposalStatus = serde_json::from_value(json!("executed")).expect("e");
    let cancelled: BlockTradeProposalStatus =
        serde_json::from_value(json!("cancelled")).expect("c");
    let rejected: BlockTradeProposalStatus = serde_json::from_value(json!("rejected")).expect("r");
    let unknown: BlockTradeProposalStatus = serde_json::from_value(json!("weird")).expect("u");
    assert_eq!(pending, BlockTradeProposalStatus::Pending);
    assert_eq!(executed, BlockTradeProposalStatus::Executed);
    assert_eq!(cancelled, BlockTradeProposalStatus::Cancelled);
    assert_eq!(rejected, BlockTradeProposalStatus::Rejected);
    assert_eq!(unknown, BlockTradeProposalStatus::Other("weird".into()));
}

#[test]
fn block_trade_proposal_fixture_decodes_every_field() {
    let proposal: BlockTradeProposal =
        serde_json::from_value(fixture("block_trade_proposal")).expect("decode");
    assert_eq!(proposal.proposal_id, PROPOSAL_ID);
    assert_eq!(proposal.proposer_user_id, "user-proposer-1");
    assert_eq!(proposal.buyer_user_id, "user-buyer-1");
    assert_eq!(proposal.buyer_subtrader_id, Some("st-buyer-1".into()));
    assert_eq!(proposal.seller_user_id, "user-seller-1");
    assert_eq!(proposal.seller_subtrader_id, Some("st-seller-1".into()));
    assert_eq!(proposal.market_ticker, "KXAPRIL-25FEB-YES");
    assert_eq!(proposal.price_centi_cents, 5400);
    assert_eq!(proposal.centicount, 100000);
    assert_eq!(proposal.maker_side, OutcomeSide::Yes);
    assert_eq!(
        proposal.expiration_ts,
        chrono::Utc.with_ymd_and_hms(2025, 2, 11, 12, 0, 0).unwrap()
    );
    assert_eq!(proposal.status, BlockTradeProposalStatus::Executed);
    assert_eq!(
        proposal.created_ts,
        chrono::Utc.with_ymd_and_hms(2025, 2, 11, 10, 0, 0).unwrap()
    );
    assert_eq!(
        proposal.updated_ts,
        chrono::Utc
            .with_ymd_and_hms(2025, 2, 11, 10, 30, 0)
            .unwrap()
    );
    assert!(proposal.buyer_accepted);
    assert!(proposal.seller_accepted);
    assert_eq!(
        proposal.buyer_accepted_ts,
        Some(
            chrono::Utc
                .with_ymd_and_hms(2025, 2, 11, 10, 10, 0)
                .unwrap()
        )
    );
    assert_eq!(
        proposal.seller_accepted_ts,
        Some(
            chrono::Utc
                .with_ymd_and_hms(2025, 2, 11, 10, 20, 0)
                .unwrap()
        )
    );
    assert_eq!(
        proposal.executed_ts,
        Some(
            chrono::Utc
                .with_ymd_and_hms(2025, 2, 11, 10, 30, 0)
                .unwrap()
        )
    );
    assert_eq!(proposal.buyer_order_id, Some("o-block-buyer-1".into()));
    assert_eq!(proposal.seller_order_id, Some("o-block-seller-1".into()));
}

#[test]
fn block_trade_proposal_missing_optional_fields_defaults() {
    let minimal: BlockTradeProposal = serde_json::from_value(json!({
        "id": "btp-min",
        "proposer_user_id": "p1",
        "buyer_user_id": "b1",
        "seller_user_id": "s1",
        "market_ticker": "KX-1-YES",
        "price_centi_cents": 1,
        "centicount": 1,
        "maker_side": "no",
        "expiration_ts": "2025-02-11T12:00:00Z",
        "status": "pending",
        "created_ts": "2025-02-11T10:00:00Z",
        "updated_ts": "2025-02-11T10:00:00Z",
        "buyer_accepted": false,
        "seller_accepted": false
    }))
    .expect("decode");
    assert_eq!(minimal.proposal_id, "btp-min");
    assert_eq!(minimal.maker_side, OutcomeSide::No);
    assert_eq!(minimal.status, BlockTradeProposalStatus::Pending);
    assert!(minimal.buyer_subtrader_id.is_none());
    assert!(minimal.seller_subtrader_id.is_none());
    assert!(minimal.buyer_accepted_ts.is_none());
    assert!(minimal.seller_accepted_ts.is_none());
    assert!(minimal.executed_ts.is_none());
    assert!(minimal.buyer_order_id.is_none());
    assert!(minimal.seller_order_id.is_none());
}

#[test]
fn created_block_trade_proposal_fixture_decodes() {
    let created: CreatedBlockTradeProposal =
        serde_json::from_value(fixture("created_block_trade_proposal")).expect("decode");
    assert_eq!(created.block_trade_proposal_id, PROPOSAL_ID);
}

#[tokio::test]
async fn list_block_trade_proposals_follows_cursors_and_sends_the_filters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals"
        )))
        .and(query_param_is_missing("cursor"))
        .and(query_param("market_ticker", "KXAPRIL-25FEB-YES"))
        .and(query_param("status", "pending"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "block_trade_proposals": [block_trade_proposal_with_id("btp-a")],
            "cursor": "c1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals"
        )))
        .and(query_param("cursor", "c1"))
        .and(query_param("market_ticker", "KXAPRIL-25FEB-YES"))
        .and(query_param("status", "pending"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "block_trade_proposals": [block_trade_proposal_with_id("btp-b")],
            "cursor": ""
        })))
        .expect(1)
        .mount(&server)
        .await;
    let filter = BlockTradeProposalFilter {
        market_ticker: Some("KXAPRIL-25FEB-YES".into()),
        status: Some(BlockTradeProposalStatus::Pending),
    };
    let proposals = client(&server, true)
        .list_block_trade_proposals(&filter, 1, 100)
        .await
        .expect("proposals");
    let ids: Vec<_> = proposals.iter().map(|p| p.proposal_id.as_str()).collect();
    assert_eq!(ids, ["btp-a", "btp-b"]);
}

#[tokio::test]
async fn block_trade_proposal_reads_require_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "block_trade_proposals": [],
            "cursor": ""
        })))
        .mount(&server)
        .await;
    let error = client(&server, false)
        .list_block_trade_proposals(&BlockTradeProposalFilter::default(), 10, 100)
        .await
        .expect_err("no credentials");
    assert!(matches!(error, KalshiError::Auth(_)), "{error:?}");
}

#[tokio::test]
async fn propose_block_trade_sends_a_signed_request_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let created = fixture("created_block_trade_proposal");
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals"
        )))
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
                && body["price_centi_cents"] == 5400
                && body["centicount"] == 100000
                && body["maker_side"] == "yes"
                && body["buyer_subaccount"] == 2
                && body["expiration_ts"] == "2025-02-11T12:00:00Z";
            let signed_ok = header(furball::ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!("{API_PREFIX}/communications/block-trade-proposals"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 })
                .set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .propose_block_trade(&new_block_trade_proposal())
        .await
        .expect("created")
    {
        WriteOutcome::Sent(created) => assert_eq!(created.block_trade_proposal_id, PROPOSAL_ID),
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn propose_block_trade_5xx_surfaces_ambiguity() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals"
        )))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .propose_block_trade(&new_block_trade_proposal())
        .await
        .expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[tokio::test]
async fn cancel_block_trade_proposal_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}"
        )))
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
                &format!("{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .cancel_block_trade_proposal(PROPOSAL_ID)
        .await
        .expect("cancelled")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn accept_block_trade_proposal_sends_the_booking_and_returns_unit() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/accept"
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
            let valid = body["subaccount"] == 4
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!(
                        "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/accept"
                    ),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let booking = AcceptBlockTradeProposal {
        subtrader_id: None,
        subaccount: Some(4),
    };
    match client
        .accept_block_trade_proposal(PROPOSAL_ID, Some(&booking))
        .await
        .expect("accepted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn accept_block_trade_proposal_without_a_booking_sends_no_body() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/accept"
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
                    "POST",
                    &format!(
                        "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/accept"
                    ),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .accept_block_trade_proposal(PROPOSAL_ID, None)
        .await
        .expect("accepted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn reject_block_trade_proposal_sends_a_signed_post_without_a_body() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/reject"
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
                    "POST",
                    &format!(
                        "{API_PREFIX}/communications/block-trade-proposals/{PROPOSAL_ID}/reject"
                    ),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    match client
        .reject_block_trade_proposal(PROPOSAL_ID)
        .await
        .expect("rejected")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}
