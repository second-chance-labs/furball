#![allow(dead_code)]

use std::sync::OnceLock;
use std::time::Duration;

use furball::{Client, Credentials};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use serde::Deserialize;
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

pub fn credentials() -> Credentials {
    Credentials::from_pem(KEY_ID, &pem()).expect("credentials")
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
