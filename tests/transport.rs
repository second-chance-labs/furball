mod common;

use std::time::{Duration, Instant};

use common::{API_PREFIX, Item, client};
use furball::{KalshiError, MAX_ATTEMPTS, Request};
use futures::TryStreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
