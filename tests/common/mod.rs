#![allow(dead_code)]

pub mod demo;

use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::SigningKey as Ed25519SigningKey;
use furball::{Client, Credentials};
use rand::Rng;
use rsa::RsaPrivateKey;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::pss::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use serde::Deserialize;
use sha2::Sha256;
use wiremock::MockServer;

pub const KEY_ID: &str = "test-key-id-1234";
pub const API_PREFIX: &str = "/trade-api/v2";

pub fn private_key() -> &'static RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| {
        RsaPrivateKey::new(&mut rand::thread_rng(), 1024).expect("generate test key")
    })
}

pub fn pem() -> String {
    private_key()
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode pem")
        .to_string()
}

pub fn pkcs1_pem() -> String {
    private_key()
        .to_pkcs1_pem(LineEnding::LF)
        .expect("encode pem")
        .to_string()
}

pub fn credentials() -> Credentials {
    Credentials::from_pem(KEY_ID, &pem()).expect("credentials")
}

pub fn ed25519_key() -> &'static Ed25519SigningKey {
    static KEY: OnceLock<Ed25519SigningKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill(&mut bytes);
        Ed25519SigningKey::from_bytes(&bytes)
    })
}

pub fn ed25519_pem() -> String {
    ed25519_key()
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode pem")
        .to_string()
}

pub fn ed25519_credentials() -> Credentials {
    Credentials::from_pem(KEY_ID, &ed25519_pem()).expect("credentials")
}

pub fn verify_rsa(timestamp: &str, http_method: &str, signed_path: &str, signature: &str) -> bool {
    let verifying_key = VerifyingKey::<Sha256>::new(private_key().to_public_key());
    let bytes = match STANDARD.decode(signature) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let signature = match Signature::try_from(bytes.as_slice()) {
        Ok(signature) => signature,
        Err(_) => return false,
    };
    verifying_key
        .verify(
            format!("{timestamp}{http_method}{signed_path}").as_bytes(),
            &signature,
        )
        .is_ok()
}

pub fn client(server: &MockServer, with_credentials: bool) -> Client {
    let mut builder = Client::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1));
    if with_credentials {
        builder = builder.credentials(credentials());
    }
    builder.build().expect("client")
}

pub fn client_with(server: &MockServer, credentials: Credentials) -> Client {
    Client::builder()
        .base_url(format!("{}{API_PREFIX}", server.uri()))
        .backoff_base(Duration::from_millis(1))
        .credentials(credentials)
        .build()
        .expect("client")
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Trade {
    pub trade_id: String,
}

impl furball::TradeId for Trade {
    fn trade_id(&self) -> &str {
        &self.trade_id
    }
}
