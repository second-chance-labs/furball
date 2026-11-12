#![cfg(feature = "trading")]
#![allow(dead_code)]

mod common;

use std::time::Duration;

use common::{API_PREFIX, KEY_ID, credentials, verify_rsa as verify};
use furball::{
    AmendOrder, CreateOrderV2, DecreaseAmount, KalshiError, OrderBuilder, OrderSide,
    PLACE_AND_CONFIRM_MAX_TRIES, SelfTradePrevention, TimeInForce, TradingClient, WriteOutcome,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

fn order(client_order_id: Option<&str>) -> CreateOrderV2 {
    let mut builder = OrderBuilder::new(
        "KXUPDOWN-25DEC",
        OrderSide::Bid,
        10.into(),
        "0.5500".parse().expect("price"),
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::TakerAtCross,
    );
    match client_order_id {
        Some(id) => builder = builder.client_order_id(id),
        None => builder = builder.without_client_order_id(),
    }
    builder.build()
}

fn trading_client(server: &MockServer) -> TradingClient {
    TradingClient::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials())
        .build()
        .expect("client")
}

struct Recorder {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl tracing::Subscriber for Recorder {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        *meta.level() <= tracing::Level::INFO
    }

    fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = String::new();
        event.record(&mut FieldVisitor(&mut fields));
        self.lines.lock().expect("recorder").push(format!(
            "{} {}",
            event.metadata().target(),
            fields
        ));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

struct FieldVisitor<'a>(&'a mut String);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .push_str(&format!("[{name}]{value:?} ", name = field.name()));
    }
}

#[test]
fn demo_builds_without_opt_in_and_production_requires_real_money() {
    let demo = TradingClient::builder()
        .credentials(credentials())
        .build()
        .expect("demo");
    assert_eq!(demo.environment(), furball::Environment::Demo);
    assert!(!demo.is_dry_run());

    let error = TradingClient::builder()
        .environment(furball::Environment::Production)
        .credentials(credentials())
        .build()
        .expect_err("production needs the opt-in");
    assert!(matches!(error, KalshiError::TradingDisabled(_)));

    let prod = TradingClient::builder()
        .environment(furball::Environment::Production)
        .allow_real_money()
        .credentials(credentials())
        .build()
        .expect("production with opt-in");
    assert_eq!(prod.environment(), furball::Environment::Production);
}

#[test]
fn build_requires_credentials() {
    let error = TradingClient::builder()
        .build()
        .expect_err("needs credentials");
    assert!(matches!(error, KalshiError::Auth(_)));
}

static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn created_order_response() -> ResponseTemplate {
    ResponseTemplate::new(201).set_body_json(json!({
        "order_id": "o1",
        "fill_count": "0",
        "remaining_count": "10",
        "ts_ms": 1
    }))
}

#[tokio::test]
async fn halted_clients_make_zero_http_requests() {
    let _serial = WRITES.lock().await;
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    let client_a = trading_client(&server_a);
    let client_b = trading_client(&server_b);
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(created_order_response())
        .mount(&server_a)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server_b)
        .await;

    TradingClient::halt();
    let error = client_a
        .create_order(&order(None))
        .await
        .expect_err("halted");
    assert!(matches!(error, KalshiError::TradingHalted));
    let error = client_b.cancel_order("o1").await.expect_err("halted");
    assert!(matches!(error, KalshiError::TradingHalted));
    assert!(server_a.received_requests().await.expect("log").is_empty());
    assert!(server_b.received_requests().await.expect("log").is_empty());

    TradingClient::resume();
    let outcome = client_a.create_order(&order(None)).await.expect("resumed");
    assert!(matches!(outcome, WriteOutcome::Sent(_)));
    assert_eq!(server_a.received_requests().await.expect("log").len(), 1);
    assert!(server_b.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn halt_during_rate_limit_backoff_stops_the_retry() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let pending = tokio::spawn(async move { client.create_order(&order(Some("c1"))).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    TradingClient::halt();
    let result = pending.await.expect("task");
    TradingClient::resume();
    assert!(matches!(result, Err(KalshiError::TradingHalted)));
    assert_eq!(server.received_requests().await.expect("log").len(), 1);
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

    let outcome = client
        .create_order(&order(Some("co-1")))
        .await
        .expect("dry run");
    match outcome {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "POST");
            assert_eq!(dry.path, "/portfolio/events/orders");
            assert_eq!(dry.cost, 10);
            assert!(dry.timestamp_ms > 0);
            assert_eq!(dry.body["ticker"], "KXUPDOWN-25DEC");
            assert_eq!(dry.body["side"], "bid");
            assert_eq!(dry.body["count"], "10");
            assert_eq!(dry.body["price"], "0.5500");
            assert_eq!(dry.body["client_order_id"], "co-1");
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }

    let outcome = client.cancel_order("o9").await.expect("dry run");
    match outcome {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "DELETE");
            assert_eq!(dry.path, "/portfolio/events/orders/o9");
            assert_eq!(dry.body, Value::Null);
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }

    assert!(server.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn create_order_sends_a_signed_request_and_decodes_the_response() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .and(header_exists(furball::ACCESS_KEY_HEADER))
        .and(header_exists(furball::ACCESS_TIMESTAMP_HEADER))
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
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["ticker"] == "KXUPDOWN-25DEC"
                && body["side"] == "bid"
                && body["count"] == "10"
                && body["price"] == "0.5500"
                && body["time_in_force"] == "good_till_canceled"
                && body["self_trade_prevention_type"] == "taker_at_cross"
                && body["client_order_id"] == "co-1";
            let signed_ok = header(furball::ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(furball::ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!("{API_PREFIX}/portfolio/events/orders"),
                    &header(furball::ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if body_ok && signed_ok { 201 } else { 401 }).set_body_json(
                json!({
                    "order_id": "o1",
                    "client_order_id": "co-1",
                    "fill_count": "0",
                    "remaining_count": "10",
                    "ts_ms": 1_700_000_000_000i64
                }),
            )
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let outcome = client
        .create_order(&order(Some("co-1")))
        .await
        .expect("created");
    match outcome {
        WriteOutcome::Sent(sent) => {
            assert_eq!(sent.order_id, "o1");
            assert_eq!(sent.client_order_id, Some("co-1".into()));
            assert_eq!(sent.fill_count, Decimal::ZERO);
            assert_eq!(sent.remaining_count, Decimal::new(10, 0));
            assert_eq!(sent.average_fill_price, None);
            assert_eq!(sent.average_fee_paid, None);
            assert_eq!(sent.ts_ms, 1_700_000_000_000);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_order_without_client_order_id_surfaces_ambiguity_on_5xx() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .create_order(&order(None))
        .await
        .expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[tokio::test]
async fn create_order_with_client_order_id_surfaces_plain_errors_on_5xx() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let error = client
        .create_order(&order(Some("co-2")))
        .await
        .expect_err("idempotent write");
    assert!(
        matches!(error, KalshiError::Api { status, .. } if status.as_u16() == 500),
        "{error:?}"
    );
}

#[tokio::test]
async fn cancel_order_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders/o1")))
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
                &format!("{API_PREFIX}/portfolio/events/orders/o1"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 200 } else { 401 }).set_body_json(json!({
                "order_id": "o1",
                "client_order_id": "co-1",
                "reduced_by": "10"
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.cancel_order("o1").await.expect("cancelled") {
        WriteOutcome::Sent(cancelled) => {
            assert_eq!(cancelled.order_id, "o1");
            assert_eq!(cancelled.client_order_id, Some("co-1".into()));
            assert_eq!(cancelled.reduced_by, Decimal::new(10, 0));
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn cancel_all_sends_a_signed_delete_scoped_to_one_subaccount() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .and(query_param("subaccount", "2"))
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
                &format!("{API_PREFIX}/portfolio/events/orders"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 204 } else { 401 })
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.cancel_all(2).await.expect("cancelled all") {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn amend_order_sends_the_required_fields_and_decodes_the_response() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/o1/amend"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["ticker"] == "KXUPDOWN-25DEC"
                && body["side"] == "bid"
                && body["price"] == "0.6000"
                && body["count"] == "12"
                && body["client_order_id"] == "co-1"
                && body["updated_client_order_id"] == "co-2"
                && body["expiration_time"] == 1_767_000_000i64;
            ResponseTemplate::new(if body_ok { 200 } else { 400 }).set_body_json(json!({
                "order_id": "o1",
                "client_order_id": "co-2",
                "remaining_count": "10",
                "fill_count": "2",
                "average_fill_price": "0.60",
                "average_fee_paid": "0.01",
                "ts_ms": 1_700_000_000_000i64
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let amend = AmendOrder {
        ticker: "KXUPDOWN-25DEC".into(),
        side: OrderSide::Bid,
        price: "0.6000".parse().expect("price"),
        count: 12.into(),
        client_order_id: Some("co-1".into()),
        updated_client_order_id: Some("co-2".into()),
        expiration_time: Some(1_767_000_000),
    };
    match client.amend_order("o1", &amend).await.expect("amended") {
        WriteOutcome::Sent(amended) => {
            assert_eq!(amended.order_id, "o1");
            assert_eq!(amended.client_order_id, Some("co-2".into()));
            assert_eq!(amended.remaining_count, Some(Decimal::new(10, 0)));
            assert_eq!(amended.fill_count, Some(Decimal::new(2, 0)));
            assert_eq!(
                amended.average_fill_price,
                Some("0.60".parse().expect("decimal"))
            );
            assert_eq!(
                amended.average_fee_paid,
                Some("0.01".parse().expect("decimal"))
            );
            assert_eq!(amended.ts_ms, 1_700_000_000_000);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn amend_order_omits_unset_optional_fields() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/o1/amend"
        )))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["ticker"] == "KXUPDOWN-25DEC"
                && body["side"] == "ask"
                && body["price"] == "0.40"
                && body["count"] == "8"
                && body.get("client_order_id").is_none()
                && body.get("updated_client_order_id").is_none()
                && body.get("expiration_time").is_none();
            ResponseTemplate::new(if body_ok { 200 } else { 400 }).set_body_json(json!({
                "order_id": "o1",
                "ts_ms": 1_700_000_000_000i64
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let amend = AmendOrder {
        ticker: "KXUPDOWN-25DEC".into(),
        side: OrderSide::Ask,
        price: "0.40".parse().expect("price"),
        count: 8.into(),
        client_order_id: None,
        updated_client_order_id: None,
        expiration_time: None,
    };
    match client.amend_order("o1", &amend).await.expect("amended") {
        WriteOutcome::Sent(amended) => {
            assert_eq!(amended.order_id, "o1");
            assert_eq!(amended.client_order_id, None);
            assert_eq!(amended.remaining_count, None);
            assert_eq!(amended.fill_count, None);
            assert_eq!(amended.average_fill_price, None);
            assert_eq!(amended.average_fee_paid, None);
            assert_eq!(amended.ts_ms, 1_700_000_000_000);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn decrease_order_serializes_exactly_one_of_reduce_by_or_reduce_to() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/o1/decrease"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let by = body.get("reduce_by").and_then(Value::as_str);
            let to = body.get("reduce_to").and_then(Value::as_str);
            let body_ok =
                (by == Some("2.00") && to.is_none()) || (by.is_none() && to == Some("3.00"));
            ResponseTemplate::new(if body_ok { 200 } else { 400 }).set_body_json(json!({
                "order_id": "o1",
                "client_order_id": "co-1",
                "remaining_count": "8.00",
                "ts_ms": 1_700_000_000_000i64
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let outcome = client
        .decrease_order("o1", &DecreaseAmount::By("2.00".parse().expect("decimal")))
        .await
        .expect("decreased");
    match outcome {
        WriteOutcome::Sent(decreased) => {
            assert_eq!(decreased.order_id, "o1");
            assert_eq!(decreased.client_order_id, Some("co-1".into()));
            assert_eq!(decreased.remaining_count, "8.00".parse().expect("decimal"));
            assert_eq!(decreased.ts_ms, 1_700_000_000_000);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
    let outcome = client
        .decrease_order("o1", &DecreaseAmount::To("3.00".parse().expect("decimal")))
        .await
        .expect("decreased");
    match outcome {
        WriteOutcome::Sent(decreased) => {
            assert_eq!(decreased.order_id, "o1");
            assert_eq!(decreased.remaining_count, "8.00".parse().expect("decimal"));
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

fn batch_item(id: &str) -> CreateOrderV2 {
    OrderBuilder::new(
        "KXUPDOWN-25DEC",
        OrderSide::Bid,
        1.into(),
        "0.55".parse().expect("price"),
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::TakerAtCross,
    )
    .client_order_id(id)
    .build()
}

#[tokio::test]
async fn batch_create_orders_returns_one_result_per_item_on_full_success() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/batched"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let orders = body["orders"].as_array().expect("orders array");
            let body_ok = orders.len() == 2
                && orders[0]["ticker"] == "KXUPDOWN-25DEC"
                && orders[0]["client_order_id"] == "co-b1"
                && orders[0]["count"] == "1"
                && orders[0]["price"] == "0.55"
                && orders[1]["client_order_id"] == "co-b2";
            ResponseTemplate::new(if body_ok { 201 } else { 400 }).set_body_json(json!({
                "orders": [
                    {
                        "order_id": "o1",
                        "client_order_id": "co-b1",
                        "fill_count": "0",
                        "remaining_count": "1",
                        "ts_ms": 1_700_000_000_000i64
                    },
                    {
                        "order_id": "o2",
                        "client_order_id": "co-b2",
                        "fill_count": "1",
                        "remaining_count": "0",
                        "average_fill_price": "0.55",
                        "average_fee_paid": "0.001",
                        "ts_ms": 1_700_000_000_100i64
                    }
                ]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let orders = [batch_item("co-b1"), batch_item("co-b2")];
    match client
        .batch_create_orders(&orders)
        .await
        .expect("batch created")
    {
        WriteOutcome::Sent(results) => {
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].order_id.as_deref(), Some("o1"));
            assert_eq!(results[0].client_order_id.as_deref(), Some("co-b1"));
            assert_eq!(results[0].fill_count, Some(Decimal::ZERO));
            assert_eq!(results[0].remaining_count, Some(Decimal::ONE));
            assert_eq!(results[0].ts_ms, Some(1_700_000_000_000));
            assert_eq!(results[0].error, None);
            assert_eq!(results[1].order_id.as_deref(), Some("o2"));
            assert_eq!(
                results[1].average_fill_price,
                Some("0.55".parse().expect("decimal"))
            );
            assert_eq!(results[1].error, None);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn batch_create_reports_failed_items_alongside_successful_ones() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/batched"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "orders": [
                {
                    "order_id": "o1",
                    "client_order_id": "co-b1",
                    "fill_count": "0",
                    "remaining_count": "1",
                    "ts_ms": 1_700_000_000_000i64
                },
                {
                    "client_order_id": "co-b2",
                    "error": { "code": 4001, "message": "insufficient funds" }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let orders = [batch_item("co-b1"), batch_item("co-b2")];
    match client
        .batch_create_orders(&orders)
        .await
        .expect("batch created")
    {
        WriteOutcome::Sent(results) => {
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].order_id.as_deref(), Some("o1"));
            assert_eq!(results[0].error, None);
            assert_eq!(results[1].order_id, None);
            assert_eq!(results[1].ts_ms, None);
            let error = results[1].error.as_ref().expect("second item failed");
            assert_eq!(error.code, 4001);
            assert_eq!(error.message, "insufficient funds");
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn batch_create_over_the_write_bucket_capacity_is_rejected_before_sending() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = trading_client(&server);
    // Basic tier: 10 tokens per item, write bucket capacity 100. Eleven items cost 110.
    let orders: Vec<CreateOrderV2> = (0..11)
        .map(|index| batch_item(&format!("co-{index}")))
        .collect();
    let error = client
        .batch_create_orders(&orders)
        .await
        .expect_err("batch too large");
    assert!(
        matches!(
            error,
            KalshiError::CostExceedsBucket {
                cost: 110,
                capacity: 100
            }
        ),
        "{error:?}"
    );
    assert!(server.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn batch_dry_run_charges_the_sum_of_its_items() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = TradingClient::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials())
        .dry_run(true)
        .build()
        .expect("client");

    let orders = [
        batch_item("co-b1"),
        batch_item("co-b2"),
        batch_item("co-b3"),
    ];
    match client.batch_create_orders(&orders).await.expect("dry run") {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.path, "/portfolio/events/orders/batched");
            assert_eq!(dry.cost, 30);
            assert_eq!(dry.body["orders"].as_array().expect("orders").len(), 3);
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }

    let cancels = [
        furball::BatchCancelOrder {
            order_id: "o1".into(),
            subaccount: None,
            exchange_index: None,
            market_ticker: None,
        },
        furball::BatchCancelOrder {
            order_id: "o2".into(),
            subaccount: Some(0),
            exchange_index: None,
            market_ticker: Some("KXUPDOWN-25DEC".into()),
        },
    ];
    match client.batch_cancel_orders(&cancels).await.expect("dry run") {
        WriteOutcome::DryRun(dry) => {
            assert_eq!(dry.method, "DELETE");
            assert_eq!(dry.path, "/portfolio/events/orders/batched");
            assert_eq!(dry.cost, 4);
            let orders = dry.body["orders"].as_array().expect("orders");
            assert_eq!(orders[0]["order_id"], "o1");
            assert!(orders[0].get("subaccount").is_none());
            assert!(orders[0].get("market_ticker").is_none());
            assert_eq!(orders[1]["subaccount"], 0);
            assert_eq!(orders[1]["market_ticker"], "KXUPDOWN-25DEC");
        }
        WriteOutcome::Sent(_) => panic!("dry run must not send"),
    }
    assert!(server.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn batch_cancel_reports_failed_items_alongside_successful_ones() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/batched"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let orders = body["orders"].as_array().expect("orders array");
            let body_ok = orders.len() == 2
                && orders[0]["order_id"] == "o1"
                && orders[1]["order_id"] == "o2"
                && orders[1]["market_ticker"] == "KXUPDOWN-25DEC";
            ResponseTemplate::new(if body_ok { 200 } else { 400 }).set_body_json(json!({
                "orders": [
                    {
                        "order_id": "o1",
                        "client_order_id": "co-b1",
                        "reduced_by": "1",
                        "ts_ms": 1_700_000_000_000i64
                    },
                    {
                        "order_id": "o2",
                        "reduced_by": "0",
                        "error": { "code": "4004", "message": "order not found" }
                    }
                ]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let cancels = [
        furball::BatchCancelOrder {
            order_id: "o1".into(),
            subaccount: None,
            exchange_index: None,
            market_ticker: None,
        },
        furball::BatchCancelOrder {
            order_id: "o2".into(),
            subaccount: None,
            exchange_index: None,
            market_ticker: Some("KXUPDOWN-25DEC".into()),
        },
    ];
    match client
        .batch_cancel_orders(&cancels)
        .await
        .expect("batch cancelled")
    {
        WriteOutcome::Sent(results) => {
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].order_id, "o1");
            assert_eq!(results[0].reduced_by, Decimal::ONE);
            assert_eq!(results[0].ts_ms, Some(1_700_000_000_000));
            assert_eq!(results[0].error, None);
            assert_eq!(results[1].order_id, "o2");
            assert_eq!(results[1].reduced_by, Decimal::ZERO);
            assert_eq!(results[1].ts_ms, None);
            let error = results[1].error.as_ref().expect("second item failed");
            assert_eq!(error.code, 4004);
            assert_eq!(error.message, "order not found");
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn batch_cancel_over_the_write_bucket_capacity_is_rejected_before_sending() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let client = trading_client(&server);
    // Basic tier: 2 tokens per item, write bucket capacity 100. Fifty-one items cost 102.
    let cancels: Vec<furball::BatchCancelOrder> = (0..51)
        .map(|index| furball::BatchCancelOrder {
            order_id: format!("o{index}"),
            subaccount: None,
            exchange_index: None,
            market_ticker: None,
        })
        .collect();
    let error = client
        .batch_cancel_orders(&cancels)
        .await
        .expect_err("batch too large");
    assert!(
        matches!(
            error,
            KalshiError::CostExceedsBucket {
                cost: 102,
                capacity: 100
            }
        ),
        "{error:?}"
    );
    assert!(server.received_requests().await.expect("log").is_empty());
}

#[tokio::test]
async fn batch_create_with_a_missing_client_order_id_surfaces_ambiguity_on_5xx() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/batched"
        )))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = trading_client(&server);
    let mut order = batch_item("co-b1");
    order.client_order_id = None;
    let error = client
        .batch_create_orders(std::slice::from_ref(&order))
        .await
        .expect_err("ambiguous");
    assert!(matches!(error, KalshiError::Ambiguous(_)), "{error:?}");
}

#[test]
fn fixture_decodes_the_full_create_order_response() {
    let file = format!(
        "{}/tests/fixtures/created_order.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let value: Value = serde_json::from_str(&std::fs::read_to_string(file).expect("fixture"))
        .expect("fixture json");
    let order: furball::Order = serde_json::from_value(value).expect("decode");
    assert_eq!(order.order_id, "o-456");
    assert_eq!(order.client_order_id, Some("c-456".into()));
    assert_eq!(
        order.fill_count,
        "5.00".parse::<Decimal>().expect("decimal")
    );
    assert_eq!(order.remaining_count, Decimal::new(5, 0));
    assert_eq!(
        order.average_fill_price,
        Some("0.55".parse().expect("decimal"))
    );
    assert_eq!(
        order.average_fee_paid,
        Some("0.01".parse().expect("decimal"))
    );
    assert_eq!(order.ts_ms, 1_767_000_000_000);
}

#[test]
fn request_body_matches_kalshi_schema_for_every_time_in_force() {
    let cases = [
        (TimeInForce::FillOrKill, "fill_or_kill"),
        (TimeInForce::GoodTillCanceled, "good_till_canceled"),
        (TimeInForce::ImmediateOrCancel, "immediate_or_cancel"),
    ];
    for (time_in_force, wire) in cases {
        let order = OrderBuilder::new(
            "KXUPDOWN-25DEC",
            OrderSide::Ask,
            7.into(),
            "0.30".parse().expect("price"),
            time_in_force,
            SelfTradePrevention::Maker,
        )
        .build();
        let body = serde_json::to_value(&order).expect("serialize");
        assert_eq!(body["ticker"], "KXUPDOWN-25DEC");
        assert_eq!(body["side"], "ask");
        assert_eq!(body["count"], "7");
        assert_eq!(body["price"], "0.30");
        assert_eq!(body["time_in_force"], wire);
        assert_eq!(body["self_trade_prevention_type"], "maker");
        assert!(
            body["client_order_id"].is_string(),
            "a client_order_id is generated by default"
        );
        assert!(body.get("post_only").is_none());
        assert!(body.get("reduce_only").is_none());
        assert!(body.get("expiration_time").is_none());
        assert!(body.get("subaccount").is_none());
        assert!(body.get("order_group_id").is_none());
        assert!(body.get("exchange_index").is_none());
    }
}

#[test]
fn builder_sets_the_optional_fields() {
    let order = OrderBuilder::new(
        "KXUPDOWN-25DEC",
        OrderSide::Bid,
        3.into(),
        "0.45".parse().expect("price"),
        TimeInForce::ImmediateOrCancel,
        SelfTradePrevention::TakerAtCross,
    )
    .client_order_id("mine-1")
    .post_only(true)
    .reduce_only(false)
    .expiration_time(1_767_000_000_000)
    .subaccount(2)
    .order_group_id("g1")
    .exchange_index(3)
    .build();
    let body = serde_json::to_value(&order).expect("serialize");
    assert_eq!(body["client_order_id"], "mine-1");
    assert_eq!(body["post_only"], true);
    assert_eq!(body["reduce_only"], false);
    assert_eq!(body["expiration_time"], 1_767_000_000_000i64);
    assert_eq!(body["subaccount"], 2);
    assert_eq!(body["order_group_id"], "g1");
    assert_eq!(body["exchange_index"], 3);
}

#[test]
fn builder_generates_a_unique_client_order_id_by_default() {
    let base = || {
        OrderBuilder::new(
            "KXUPDOWN-25DEC",
            OrderSide::Bid,
            1.into(),
            "0.5".parse().expect("price"),
            TimeInForce::GoodTillCanceled,
            SelfTradePrevention::TakerAtCross,
        )
    };
    let first = base().build().client_order_id.expect("generated");
    let second = base().build().client_order_id.expect("generated");
    assert_ne!(first, second);
    assert_eq!(first.len(), 36);
    assert_eq!(first.chars().nth(14).expect("version nibble"), '4');
    assert!(matches!(
        first.chars().nth(19).expect("variant nibble"),
        '8' | '9' | 'a' | 'b'
    ));
}

#[tokio::test]
async fn write_attempts_are_logged_without_credentials() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "order_id": "o1",
            "fill_count": "0",
            "remaining_count": "10",
            "ts_ms": 1
        })))
        .expect(1)
        .mount(&server)
        .await;

    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(Recorder {
        lines: lines.clone(),
    })
    .expect("only this test sets the global default");

    let client = trading_client(&server);
    client
        .create_order(&order(Some("co-123")))
        .await
        .expect("sent");

    let recorded = lines.lock().expect("recorder").join("\n");
    assert!(recorded.contains("kalshi trading write"));
    assert!(recorded.contains("[method]POST"));
    assert!(recorded.contains("[path]/portfolio/events/orders"));
    assert!(recorded.contains("[ticker]KXUPDOWN-25DEC"));
    assert!(recorded.contains("[client_order_id]Some(\"co-123\")"));
    assert!(!recorded.contains(KEY_ID));
    assert!(!recorded.contains("PRIVATE KEY"));
}

fn fixture_value(name: &str) -> Value {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("fixture json")
}

#[tokio::test]
async fn create_order_group_sends_the_limit_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("order_group");
    let response_fixture = fixture.clone();
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/order_groups/create")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["contracts_limit_fp"] == "50.00"
                && body["subaccount"] == 0
                && body["exchange_index"] == 1;
            if body_ok {
                ResponseTemplate::new(201).set_body_json(response_fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let group = furball::NewOrderGroup {
        contracts_limit_fp: "50.00".parse().expect("decimal"),
        subaccount: Some(0),
        exchange_index: Some(1),
    };
    match client.create_order_group(&group).await.expect("created") {
        WriteOutcome::Sent(created) => {
            let expected: furball::OrderGroup = serde_json::from_value(fixture).expect("decode");
            assert_eq!(created, expected);
            assert_eq!(created.order_group_id, "g-123");
            assert_eq!(created.subaccount, 0);
            assert_eq!(created.exchange_index, Some(1));
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn delete_order_group_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("DELETE"))
        .and(path(format!("{API_PREFIX}/portfolio/order_groups/g-123")))
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
                &format!("{API_PREFIX}/portfolio/order_groups/g-123"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 200 } else { 401 }).set_body_json(fixture.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.delete_order_group("g-123").await.expect("deleted") {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn reset_order_group_sends_a_signed_put() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/order_groups/g-123/reset"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture))
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.reset_order_group("g-123").await.expect("reset") {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn trigger_order_group_sends_a_signed_put() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/order_groups/g-123/trigger"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture))
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .trigger_order_group("g-123")
        .await
        .expect("triggered")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn update_order_group_limit_sends_the_new_limit() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/order_groups/g-123/limit"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let single_field = body.as_object().is_some_and(|map| map.len() == 1);
            let body_ok = body["contracts_limit_fp"] == "75.00" && single_field;
            if body_ok {
                ResponseTemplate::new(200).set_body_json(fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let limit = "75.00".parse::<Decimal>().expect("decimal");
    match client
        .update_order_group_limit("g-123", &limit)
        .await
        .expect("updated")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn order_builder_attaches_orders_to_a_group() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("order_group");
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/order_groups/create")))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture))
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let created = match client
        .create_order_group(&furball::NewOrderGroup {
            contracts_limit_fp: 50.into(),
            subaccount: None,
            exchange_index: None,
        })
        .await
        .expect("created")
    {
        WriteOutcome::Sent(created) => created,
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    };

    let order = OrderBuilder::new(
        "KXUPDOWN-25DEC",
        OrderSide::Bid,
        1.into(),
        "0.55".parse().expect("price"),
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::TakerAtCross,
    )
    .order_group_id(created.order_group_id)
    .build();
    let body = serde_json::to_value(&order).expect("serialize");
    assert_eq!(body["order_group_id"], "g-123");
    assert_eq!(server.received_requests().await.expect("log").len(), 1);
}

#[tokio::test]
async fn set_position_exit_trigger_sends_the_trigger_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("exit_trigger");
    let response_fixture = fixture.clone();
    Mock::given(method("PUT"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/positions/KXUPDOWN-25DEC/exit_trigger"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["kind"] == "bracket"
                && body["stop_loss_price"] == "0.45"
                && body["take_profit_price"] == "0.85"
                && body["slippage"]["unit"] == "bps"
                && body["slippage"]["value"] == 50;
            if body_ok {
                ResponseTemplate::new(200).set_body_json(response_fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let trigger = furball::ExitTrigger {
        slippage: Some(furball::ExitSlippage {
            unit: furball::SlippageUnit::Bps,
            value: 50,
        }),
        kind: Some(furball::ExitTriggerKind::Bracket),
        stop_loss_price: Some("0.45".parse().expect("decimal")),
        take_profit_price: Some("0.85".parse().expect("decimal")),
    };
    match client
        .set_position_exit_trigger("KXUPDOWN-25DEC", &trigger)
        .await
        .expect("set")
    {
        WriteOutcome::Sent(set) => {
            let expected: furball::EventContractExitTrigger =
                serde_json::from_value(fixture).expect("decode fixture");
            assert_eq!(set, expected);
            assert_eq!(set.id, "et-123");
            assert_eq!(set.ticker, "KXUPDOWN-25DEC");
            assert_eq!(set.kind, furball::ExitTriggerKind::Bracket);
            assert_eq!(set.status, furball::ExitTriggerStatus::Active);
            assert_eq!(set.stop_loss_price, Some("0.45".parse().expect("decimal")));
            assert_eq!(set.slippage.unwrap().value, 50);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn delete_position_exit_trigger_sends_a_signed_delete() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/positions/KXUPDOWN-25DEC/exit_trigger"
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
                &format!("{API_PREFIX}/portfolio/positions/KXUPDOWN-25DEC/exit_trigger"),
                &header(furball::ACCESS_SIGNATURE_HEADER),
            );
            ResponseTemplate::new(if valid { 200 } else { 401 }).set_body_json(fixture.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .delete_position_exit_trigger("KXUPDOWN-25DEC")
        .await
        .expect("deleted")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_order_with_exit_trigger_sends_order_and_trigger_and_decodes_the_order() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("created_order");
    let response_fixture = fixture.clone();
    Mock::given(method("POST"))
        .and(path(format!(
            "{API_PREFIX}/portfolio/events/orders/with_exit_trigger"
        )))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["ticker"] == "KXUPDOWN-25DEC"
                && body["side"] == "bid"
                && body["count"] == "1"
                && body["price"] == "0.55"
                && body["client_order_id"] == "co-et"
                && body["exit_trigger"]["stop_loss_price"] == "0.45"
                && body["exit_trigger"]["take_profit_price"] == "0.85"
                && body["exit_trigger"]["slippage"]["unit"] == "cents"
                && body["exit_trigger"]["slippage"]["value"] == 3;
            if body_ok {
                ResponseTemplate::new(201).set_body_json(response_fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let order = batch_item("co-et");
    let trigger = furball::ExitTrigger {
        slippage: Some(furball::ExitSlippage {
            unit: furball::SlippageUnit::Cents,
            value: 3,
        }),
        kind: None,
        stop_loss_price: Some("0.45".parse().expect("decimal")),
        take_profit_price: Some("0.85".parse().expect("decimal")),
    };
    match client
        .create_order_with_exit_trigger(&order, &trigger)
        .await
        .expect("created")
    {
        WriteOutcome::Sent(created) => {
            let expected: furball::Order = serde_json::from_value(fixture).expect("decode");
            assert_eq!(created, expected);
            assert_eq!(created.order_id, "o-456");
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn create_subaccount_sends_the_request_and_decodes_the_fixture() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("created_subaccount");
    let response_fixture = fixture.clone();
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/subaccounts")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok =
                body["exchange_index"] == 1 && body.as_object().is_some_and(|m| m.len() == 1);
            if body_ok {
                ResponseTemplate::new(201).set_body_json(response_fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client.create_subaccount(Some(1)).await.expect("created") {
        WriteOutcome::Sent(created) => {
            let expected: furball::CreatedSubaccount =
                serde_json::from_value(fixture).expect("decode fixture");
            assert_eq!(created, expected);
            assert_eq!(created.subaccount_number, 7);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn transfer_between_subaccounts_sends_the_required_fields() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/subaccounts/transfer")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["client_transfer_id"] == "t-uuid-1"
                && body["from_subaccount"] == 0
                && body["to_subaccount"] == 2
                && body["amount_cents"] == 150_000
                && body.get("exchange_index").is_none();
            if body_ok {
                ResponseTemplate::new(200).set_body_json(fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let transfer = furball::SubaccountTransfer {
        client_transfer_id: "t-uuid-1".into(),
        from_subaccount: 0,
        to_subaccount: 2,
        amount_cents: 150_000,
        exchange_index: None,
    };
    match client
        .transfer_between_subaccounts(&transfer)
        .await
        .expect("transferred")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn update_subaccount_netting_sends_the_subaccount_and_flag() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let fixture = fixture_value("empty");
    Mock::given(method("PUT"))
        .and(path(format!("{API_PREFIX}/portfolio/subaccounts/netting")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(move |request: &MockRequest| {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            let body_ok = body["subaccount_number"] == 2 && body["enabled"] == true;
            if body_ok {
                ResponseTemplate::new(200).set_body_json(fixture.clone())
            } else {
                ResponseTemplate::new(400)
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .update_subaccount_netting(2, true)
        .await
        .expect("updated")
    {
        WriteOutcome::Sent(()) => {}
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
}

#[tokio::test]
async fn place_and_confirm_success_sends_a_generated_client_order_id() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .and(header_exists(furball::ACCESS_SIGNATURE_HEADER))
        .respond_with(|request: &MockRequest| {
            let sent: Value = serde_json::from_slice(&request.body).expect("json body");
            let id = sent["client_order_id"].as_str().unwrap_or_default();
            ResponseTemplate::new(201).set_body_json(json!({
                "order_id": "o1",
                "client_order_id": id,
                "fill_count": "0",
                "remaining_count": "10",
                "ts_ms": 1_700_000_000_000i64
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .place_and_confirm(&order(None))
        .await
        .expect("confirmed")
    {
        WriteOutcome::Sent(sent) => {
            let id = sent.client_order_id.expect("echoed id");
            assert_eq!(id.len(), 36);
            assert_eq!(id.chars().nth(8), Some('-'));
            assert_eq!(id.chars().nth(13), Some('-'));
            assert_eq!(id.chars().nth(14), Some('4'));
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
    let requests = server.received_requests().await.expect("log");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
}

#[tokio::test]
async fn place_and_confirm_returns_the_found_order_after_an_ambiguous_send() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(query_param("ticker", "KXUPDOWN-25DEC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "orders": [fixture_value("portfolio_order")],
            "cursor": ""
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .place_and_confirm(&order(Some("c-confirm")))
        .await
        .expect("confirmed")
    {
        WriteOutcome::Sent(sent) => {
            assert_eq!(sent.order_id, "o-confirm");
            assert_eq!(sent.client_order_id.as_deref(), Some("c-confirm"));
            assert_eq!(sent.fill_count, Decimal::new(5, 0));
            assert_eq!(sent.remaining_count, Decimal::new(5, 0));
            assert_eq!(sent.average_fill_price, None);
            assert_eq!(sent.average_fee_paid, None);
            assert_eq!(sent.ts_ms, 1_793_531_100_000);
        }
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
    let requests = server.received_requests().await.expect("log");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method.as_str(), "GET");
}

#[tokio::test]
async fn place_and_confirm_retries_with_the_same_id_when_the_order_is_not_there() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = sends.clone();
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(move |_request: &MockRequest| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" }))
            } else {
                ResponseTemplate::new(201).set_body_json(json!({
                    "order_id": "o1",
                    "client_order_id": "c-confirm",
                    "fill_count": "0",
                    "remaining_count": "10",
                    "ts_ms": 1_700_000_000_000i64
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "orders": [], "cursor": "" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    match client
        .place_and_confirm(&order(Some("c-confirm")))
        .await
        .expect("confirmed")
    {
        WriteOutcome::Sent(sent) => assert_eq!(sent.order_id, "o1"),
        WriteOutcome::DryRun(_) => panic!("not a dry run"),
    }
    let posts: Vec<Value> = server
        .received_requests()
        .await
        .expect("log")
        .into_iter()
        .filter(|request| request.method.as_str() == "POST")
        .map(|request| serde_json::from_slice(&request.body).expect("json body"))
        .collect();
    assert_eq!(posts.len(), 2);
    let ids: Vec<&str> = posts
        .iter()
        .map(|body| body["client_order_id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["c-confirm", "c-confirm"]);
}

#[tokio::test]
async fn place_and_confirm_gives_up_after_the_fixed_number_of_ambiguous_tries() {
    let _serial = WRITES.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/events/orders")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(PLACE_AND_CONFIRM_MAX_TRIES as u64)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "orders": [], "cursor": "" })),
        )
        .expect(PLACE_AND_CONFIRM_MAX_TRIES as u64)
        .mount(&server)
        .await;

    let client = trading_client(&server);
    let error = client
        .place_and_confirm(&order(Some("c-confirm")))
        .await
        .expect_err("gives up");
    let KalshiError::Ambiguous(detail) = &error else {
        panic!("expected ambiguous, got {error:?}")
    };
    assert!(detail.contains("c-confirm"), "{detail}");
    assert!(
        detail.contains(&PLACE_AND_CONFIRM_MAX_TRIES.to_string()),
        "{detail}"
    );
    let posts: Vec<Value> = server
        .received_requests()
        .await
        .expect("log")
        .into_iter()
        .filter(|request| request.method.as_str() == "POST")
        .map(|request| serde_json::from_slice(&request.body).expect("json body"))
        .collect();
    assert_eq!(posts.len(), PLACE_AND_CONFIRM_MAX_TRIES as usize);
    for body in &posts {
        assert_eq!(body["client_order_id"].as_str(), Some("c-confirm"));
    }
}
