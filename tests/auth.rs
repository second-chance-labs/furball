mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{
    API_PREFIX, KEY_ID, client, client_with, credentials, ed25519_credentials, ed25519_key,
    ed25519_pem, pem, pkcs1_pem, private_key,
};
use ed25519_dalek::Verifier;
use furball::{
    ACCESS_KEY_HEADER, ACCESS_SIGNATURE_HEADER, ACCESS_TIMESTAMP_HEADER, Credentials, KalshiError,
    KeyType, Request,
};
use rsa::pss::{Signature, VerifyingKey};
use serde_json::Value;
use sha2::Sha256;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

fn verify(timestamp: &str, http_method: &str, signed_path: &str, signature: &str) -> bool {
    let verifying_key = VerifyingKey::<Sha256>::new(private_key().to_public_key());
    let bytes = STANDARD.decode(signature).expect("base64");
    let signature = Signature::try_from(bytes.as_slice()).expect("signature bytes");
    verifying_key
        .verify(
            format!("{timestamp}{http_method}{signed_path}").as_bytes(),
            &signature,
        )
        .is_ok()
}

fn verify_ed(timestamp: &str, http_method: &str, signed_path: &str, signature: &str) -> bool {
    let bytes = STANDARD.decode(signature).expect("base64");
    let signature = ed25519_dalek::Signature::try_from(bytes.as_slice()).expect("signature bytes");
    ed25519_key()
        .verifying_key()
        .verify(
            format!("{timestamp}{http_method}{signed_path}").as_bytes(),
            &signature,
        )
        .is_ok()
}

#[test]
fn signature_covers_timestamp_method_and_path_without_query() {
    let headers = credentials().sign(
        1_703_123_456_789,
        "GET",
        "/trade-api/v2/portfolio/balance?limit=5",
    );
    assert_eq!(headers.key_id, KEY_ID);
    assert_eq!(headers.timestamp, "1703123456789");
    assert!(verify(
        &headers.timestamp,
        "GET",
        "/trade-api/v2/portfolio/balance",
        &headers.signature
    ));
    assert!(!verify(
        &headers.timestamp,
        "POST",
        "/trade-api/v2/portfolio/balance",
        &headers.signature
    ));
    assert!(!verify(
        &headers.timestamp,
        "GET",
        "/trade-api/v2/portfolio/orders",
        &headers.signature
    ));
    assert!(!verify(
        "1703123456790",
        "GET",
        "/trade-api/v2/portfolio/balance",
        &headers.signature
    ));
}

#[tokio::test]
async fn authenticated_request_signs_the_full_path_and_public_request_sends_no_auth_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/private")))
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
            let valid = header(ACCESS_KEY_HEADER) == KEY_ID
                && verify(
                    &header(ACCESS_TIMESTAMP_HEADER),
                    "GET",
                    &format!("{API_PREFIX}/private"),
                    &header(ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 200 } else { 401 })
                .set_body_json(serde_json::json!({}))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/public")))
        .respond_with(|request: &MockRequest| {
            let leaked = [
                ACCESS_KEY_HEADER,
                ACCESS_TIMESTAMP_HEADER,
                ACCESS_SIGNATURE_HEADER,
            ]
            .iter()
            .any(|name| request.headers.contains_key(*name));
            ResponseTemplate::new(if leaked { 400 } else { 200 })
                .set_body_json(serde_json::json!({}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, true);
    client
        .send::<Value>(&Request::get("/private").query("limit", 5).authenticated())
        .await
        .expect("signed request accepted");
    client
        .send::<Value>(&Request::get("/public"))
        .await
        .expect("public request carries no auth headers");
}

#[tokio::test]
async fn authenticated_request_without_credentials_fails_before_any_http_call() {
    let server = MockServer::start().await;
    let error = client(&server, false)
        .send::<Value>(&Request::get("/private").authenticated())
        .await
        .expect_err("must fail");
    assert!(matches!(error, KalshiError::Auth(_)));
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
}

#[tokio::test]
async fn http_errors_and_debug_output_never_contain_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;
    let error = client(&server, true)
        .send::<Value>(&Request::get("/private").authenticated())
        .await
        .expect_err("403");

    let signature = credentials().sign(1, "GET", "/x").signature;
    let rendered = [
        format!("{error}"),
        format!("{error:?}"),
        format!("{:?}", credentials()),
        format!("{:?}", credentials().sign(1, "GET", "/x")),
    ]
    .join("\n");
    assert!(matches!(error, KalshiError::Http { .. }));
    assert!(!rendered.contains(KEY_ID));
    assert!(!rendered.contains(&signature));
    assert!(!rendered.contains("PRIVATE KEY"));
}

#[test]
fn invalid_pem_error_does_not_echo_the_key_material() {
    let garbage = format!("{}garbage", pem());
    let error = Credentials::from_pem(KEY_ID, &garbage).expect_err("invalid pem");
    let rendered = format!("{error} {error:?}");
    assert!(matches!(error, KalshiError::Auth(_)));
    assert!(!rendered.contains(KEY_ID));
    assert!(!rendered.contains("BEGIN"));
}

#[test]
fn from_env_loads_key_from_file_and_reports_missing_variables_by_name() {
    let directory = tempfile::tempdir().expect("tempdir");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&key_path, pem()).expect("write key");

    // SAFETY: this is the only test in the binary touching these variables.
    unsafe {
        std::env::remove_var(furball::KEY_ID_ENV);
        std::env::set_var(furball::PRIVATE_KEY_PATH_ENV, &key_path);
    }
    let error = Credentials::from_env().expect_err("key id missing");
    assert!(format!("{error}").contains(furball::KEY_ID_ENV));

    unsafe { std::env::set_var(furball::KEY_ID_ENV, KEY_ID) };
    let credentials = Credentials::from_env().expect("loads");
    let headers = credentials.sign(7, "GET", "/p");
    assert!(verify("7", "GET", "/p", &headers.signature));

    unsafe {
        std::env::set_var(
            furball::PRIVATE_KEY_PATH_ENV,
            directory.path().join("missing.pem"),
        )
    };
    let error = Credentials::from_env().expect_err("file missing");
    assert!(!format!("{error}").contains(KEY_ID));
}

#[test]
fn rsa_pkcs8_key_reports_rsa_key_type() {
    assert_eq!(credentials().key_type(), KeyType::Rsa);
}

#[test]
fn rsa_pkcs1_key_is_detected_and_signs() {
    let credentials = Credentials::from_pem(KEY_ID, &pkcs1_pem()).expect("credentials");
    assert_eq!(credentials.key_type(), KeyType::Rsa);
    let headers = credentials.sign(42, "PUT", "/trade-api/v2/portfolio/orders/x");
    assert_eq!(headers.key_id, KEY_ID);
    assert_eq!(headers.timestamp, "42");
    assert!(verify(
        "42",
        "PUT",
        "/trade-api/v2/portfolio/orders/x",
        &headers.signature
    ));
}

#[test]
fn ed25519_signature_round_trips_and_key_type_is_reported() {
    let credentials = Credentials::from_pem(KEY_ID, &ed25519_pem()).expect("credentials");
    assert_eq!(credentials.key_type(), KeyType::Ed25519);
    let headers = credentials.sign(
        1_703_123_456_789,
        "POST",
        "/trade-api/v2/portfolio/orders?limit=5",
    );
    assert_eq!(headers.key_id, KEY_ID);
    assert_eq!(headers.timestamp, "1703123456789");
    assert!(verify_ed(
        &headers.timestamp,
        "POST",
        "/trade-api/v2/portfolio/orders",
        &headers.signature
    ));
    assert!(!verify_ed(
        &headers.timestamp,
        "GET",
        "/trade-api/v2/portfolio/orders",
        &headers.signature
    ));
    assert!(!verify_ed(
        "1703123456790",
        "POST",
        "/trade-api/v2/portfolio/orders",
        &headers.signature
    ));
}

#[test]
fn ed25519_signature_fails_rsa_pss_verification() {
    let ed = Credentials::from_pem(KEY_ID, &ed25519_pem()).expect("credentials");
    let headers = ed.sign(1, "POST", "/p");
    assert!(!verify("1", "POST", "/p", &headers.signature));
}

#[tokio::test]
async fn authenticated_request_signs_with_ed25519() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API_PREFIX}/private")))
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
            let valid = header(ACCESS_KEY_HEADER) == KEY_ID
                && verify_ed(
                    &header(ACCESS_TIMESTAMP_HEADER),
                    "GET",
                    &format!("{API_PREFIX}/private"),
                    &header(ACCESS_SIGNATURE_HEADER),
                );
            ResponseTemplate::new(if valid { 200 } else { 401 })
                .set_body_json(serde_json::json!({}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = client_with(&server, ed25519_credentials());
    client
        .send::<Value>(&Request::get("/private").query("limit", 5).authenticated())
        .await
        .expect("ed25519-signed request accepted");
}

#[test]
fn ed25519_debug_and_error_output_never_contain_credentials() {
    let credentials = ed25519_credentials();
    let rendered = [
        format!("{:?}", credentials),
        format!("{:?}", credentials.sign(1, "POST", "/x")),
        format!("{:?}", KeyType::Ed25519),
    ]
    .join("\n");
    assert!(!rendered.contains(KEY_ID));
    assert!(!rendered.contains("PRIVATE KEY"));
}
