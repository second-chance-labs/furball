#![cfg(feature = "trading")]
#![allow(dead_code)]

mod common;

use std::time::Duration;

use common::{API_PREFIX, KEY_ID, credentials, verify_rsa as verify};
use furball::{
    KalshiError, NewOrder, OrderSide, SelfTradePrevention, TimeInForce, TradingClient, WriteOutcome,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

fn order(client_order_id: Option<&str>) -> NewOrder {
    NewOrder {
        ticker: "KXUPDOWN-25DEC".into(),
        side: OrderSide::Bid,
        count: 10.into(),
        price: "0.5500".parse().expect("price"),
        time_in_force: TimeInForce::GoodTillCanceled,
        self_trade_prevention_type: SelfTradePrevention::TakerAtCross,
        client_order_id: client_order_id.map(str::to_owned),
        post_only: None,
        reduce_only: None,
    }
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
