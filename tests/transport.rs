mod common;

use std::time::{Duration, Instant};

use common::verify_rsa as verify;
use common::{API_PREFIX, Item, KEY_ID, client};
use furball::{
    ACCESS_KEY_HEADER, ACCESS_SIGNATURE_HEADER, ACCESS_TIMESTAMP_HEADER, KalshiError, MAX_ATTEMPTS,
    Request,
};
use futures::TryStreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

fn page(ids: &[&str], cursor: &str) -> ResponseTemplate {
    let items: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
    ResponseTemplate::new(200).set_body_json(json!({ "markets": items, "cursor": cursor }))
}

async fn mount_three_pages(server: &MockServer) {
    let list = format!("{API_PREFIX}/markets");
    Mock::given(method("GET"))
        .and(path(&list))
        .and(query_param_is_missing("cursor"))
        .and(query_param("limit", "2"))
        .respond_with(page(&["a", "b"], "c1"))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(&list))
        .and(query_param("cursor", "c1"))
        .respond_with(page(&["c", "d"], "c2"))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(&list))
        .and(query_param("cursor", "c2"))
        .respond_with(page(&["e"], ""))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn pagination_follows_cursor_across_three_pages() {
    let server = MockServer::start().await;
    mount_three_pages(&server).await;
    let items: Vec<Item> = client(&server, false)
        .paginate(Request::get("/markets"), "markets", 2, 100)
        .try_collect()
        .await
        .expect("pages");
    let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c", "d", "e"]);
}

#[tokio::test]
async fn pagination_max_items_stops_fetching_further_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param_is_missing("cursor"))
        .respond_with(page(&["a", "b"], "c1"))
        .expect(1)
        .mount(&server)
        .await;
    let items: Vec<Item> = client(&server, false)
        .paginate(Request::get("/markets"), "markets", 2, 1)
        .try_collect()
        .await
        .expect("pages");
    assert_eq!(items, [Item { id: "a".into() }]);
}

#[tokio::test]
async fn pagination_surfaces_decode_errors_with_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "markets": [{ "nope": 1 }], "cursor": "" })),
        )
        .mount(&server)
        .await;
    let error = client(&server, false)
        .paginate::<Item>(Request::get("/markets"), "markets", 2, 10)
        .try_collect::<Vec<_>>()
        .await
        .expect_err("decode");
    assert!(matches!(&error, KalshiError::Decode { body, .. } if body.contains("nope")));
}

#[tokio::test]
async fn rate_limited_requests_retry_then_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(r#"{"error":"too many requests"}"#),
        )
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;
    let value: Value = client(&server, false)
        .send(&Request::get("/x"))
        .await
        .expect("eventually ok");
    assert_eq!(value["ok"], true);
}

#[tokio::test]
async fn rate_limit_gives_up_after_max_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .expect(u64::from(MAX_ATTEMPTS))
        .mount(&server)
        .await;
    let error = client(&server, false)
        .send::<Value>(&Request::get("/x"))
        .await
        .expect_err("exhausted");
    assert!(matches!(error, KalshiError::RateLimited));
}

#[tokio::test]
async fn retry_after_header_overrides_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let started = Instant::now();
    client(&server, false)
        .send::<Value>(&Request::get("/x"))
        .await
        .expect("ok");
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[tokio::test]
async fn status_codes_map_to_error_variants() {
    let server = MockServer::start().await;
    Mock::given(path(format!("{API_PREFIX}/missing")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path(format!("{API_PREFIX}/broken")))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    Mock::given(path(format!("{API_PREFIX}/garbled")))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let client = client(&server, false);
    assert!(matches!(
        client.send::<Value>(&Request::get("/missing")).await,
        Err(KalshiError::NotFound)
    ));
    assert!(matches!(
        client.send::<Value>(&Request::get("/broken")).await,
        Err(KalshiError::Http { status, body }) if status.as_u16() == 500 && body == "boom"
    ));
    assert!(matches!(
        client.send::<Value>(&Request::get("/garbled")).await,
        Err(KalshiError::Decode { body, .. }) if body == "not json"
    ));
}

#[tokio::test]
async fn token_bucket_delays_requests_beyond_its_capacity() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let client = common::client(&server, false);
    let started = Instant::now();
    // Basic holds 600 tokens (60 reads) and refills 200/s: 80 reads must wait for ~20 reads of refill.
    for _ in 0..80 {
        client.send::<Value>(&Request::get("/x")).await.expect("ok");
    }
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn post_sends_json_body_and_signs_path_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .and(header_exists(ACCESS_KEY_HEADER))
        .and(header_exists(ACCESS_TIMESTAMP_HEADER))
        .and(header_exists(ACCESS_SIGNATURE_HEADER))
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
            let body_ok = body["ticker"] == "KXUPDOWN"
                && body["side"] == "buy"
                && body["type"] == "limit"
                && body["limit_price_dollars"] == "0.42"
                && body["size"] == 10;
            let signed_ok = header(ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(ACCESS_TIMESTAMP_HEADER),
                    "POST",
                    &format!("{API_PREFIX}/portfolio/orders"),
                    &header(ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if body_ok && signed_ok { 200 } else { 401 })
                .set_body_json(json!({ "id": "o1" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, true);
    let response: Value = client
        .send(
            &Request::post("/portfolio/orders")
                .body(json!({
                    "ticker": "KXUPDOWN",
                    "side": "buy",
                    "type": "limit",
                    "limit_price_dollars": "0.42",
                    "size": 10
                }))
                .authenticated(),
        )
        .await
        .expect("signed post accepted");
    assert_eq!(response["id"], "o1");
}

#[tokio::test]
async fn write_429_is_retried_until_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({ "code": 429, "message": "slow down" })),
        )
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "o1" })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let response: Value = client
        .send(
            &Request::post("/portfolio/orders")
                .body(json!({ "ticker": "KXUPDOWN" }))
                .authenticated(),
        )
        .await
        .expect("eventually ok");
    assert_eq!(response["id"], "o1");
}

#[tokio::test]
async fn server_error_on_non_idempotent_write_is_ambiguous_and_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({ "code": 5000, "message": "internal error" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let error = client
        .send::<Value>(
            &Request::post("/portfolio/orders")
                .body(json!({ "ticker": "KXUPDOWN" }))
                .authenticated(),
        )
        .await
        .expect_err("ambiguous");
    assert!(
        matches!(&error, KalshiError::Ambiguous(detail)
            if detail.contains("HTTP 500") && detail.contains("code 5000: internal error")),
        "unexpected: {error:?}"
    );
}

#[tokio::test]
async fn connection_failure_is_ambiguous_for_writes_but_transport_for_get() {
    let dead = furball::Client::builder()
        .base_url("http://127.0.0.1:1/trade-api/v2")
        .build()
        .expect("client");
    let error = dead
        .send::<Value>(&Request::post("/portfolio/orders").body(json!({})))
        .await
        .expect_err("no connection");
    assert!(matches!(error, KalshiError::Ambiguous(_)));
    let error = dead
        .send::<Value>(&Request::get("/markets"))
        .await
        .expect_err("no connection");
    assert!(matches!(error, KalshiError::Transport(_)));
}

#[tokio::test]
async fn put_and_delete_use_their_methods_and_delete_is_idempotent() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("{API_PREFIX}/portfolio/orders/o1")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": "o1", "state": "filled" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{API_PREFIX}/portfolio/orders/o1")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "code": 5000, "message": "boom" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let put: Value = client
        .send(
            &Request::put("/portfolio/orders/o1")
                .body(json!({ "size": 5 }))
                .authenticated(),
        )
        .await
        .expect("put ok");
    assert_eq!(put["state"], "filled");
    let error = client
        .send::<Value>(&Request::delete("/portfolio/orders/o1").authenticated())
        .await
        .expect_err("500");
    assert!(
        matches!(error, KalshiError::Api { status, .. } if status.as_u16() == 500),
        "idempotent write surfaces a plain error, got: {error:?}"
    );
}

#[tokio::test]
async fn json_error_bodies_decode_to_the_api_variant() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders")))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "code": 1023, "message": "Order must be a limit order" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API_PREFIX}/portfolio/orders/garbled")))
        .respond_with(ResponseTemplate::new(400).set_body_string("not json"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, true);
    let error = client
        .send::<Value>(
            &Request::post("/portfolio/orders")
                .body(json!({}))
                .authenticated(),
        )
        .await
        .expect_err("400");
    assert!(
        matches!(&error,
            KalshiError::Api { status, code, message }
                if status.as_u16() == 400 && *code == 1023 && message == "Order must be a limit order"
        ),
        "unexpected: {error:?}"
    );
    let error = client
        .send::<Value>(
            &Request::post("/portfolio/orders/garbled")
                .body(json!({}))
                .authenticated(),
        )
        .await
        .expect_err("400");
    assert!(
        matches!(
            &error,
            KalshiError::Http { status, body } if status.as_u16() == 400 && body == "not json"
        ),
        "unexpected: {error:?}"
    );
}
