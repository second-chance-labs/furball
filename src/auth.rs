use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use sha2::Sha256;

use crate::error::{KalshiError, Result};

/// Environment variable holding the API key ID.
pub const KEY_ID_ENV: &str = "KALSHI_API_KEY_ID";
/// Environment variable holding the path to the PEM-encoded RSA private key.
pub const PRIVATE_KEY_PATH_ENV: &str = "KALSHI_PRIVATE_KEY_PATH";

/// Header carrying the API key ID.
pub const ACCESS_KEY_HEADER: &str = "KALSHI-ACCESS-KEY";
/// Header carrying the request timestamp in milliseconds since the Unix epoch.
pub const ACCESS_TIMESTAMP_HEADER: &str = "KALSHI-ACCESS-TIMESTAMP";
/// Header carrying the base64 RSA-PSS signature.
pub const ACCESS_SIGNATURE_HEADER: &str = "KALSHI-ACCESS-SIGNATURE";

/// The three headers Kalshi requires on an authenticated request.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    /// Value for [`ACCESS_KEY_HEADER`].
    pub key_id: String,
    /// Value for [`ACCESS_TIMESTAMP_HEADER`].
    pub timestamp: String,
    /// Value for [`ACCESS_SIGNATURE_HEADER`].
    pub signature: String,
}

impl fmt::Debug for SignedHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignedHeaders(<redacted>)")
    }
}

/// An API key ID plus its RSA private key. `Debug` never prints either.
#[derive(Clone)]
pub struct Credentials {
    key_id: String,
    signing_key: BlindedSigningKey<Sha256>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

impl Credentials {
    /// Builds credentials from an API key ID and a PEM private key (PKCS#8 or PKCS#1).
    pub fn from_pem(key_id: impl Into<String>, pem: &str) -> Result<Self> {
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .map_err(|_| KalshiError::Auth("private key is not a valid RSA PEM".into()))?;
        Ok(Self {
            key_id: key_id.into(),
            signing_key: BlindedSigningKey::<Sha256>::new(key),
        })
    }

    /// Reads [`KEY_ID_ENV`] and the PEM file named by [`PRIVATE_KEY_PATH_ENV`].
    pub fn from_env() -> Result<Self> {
        let key_id = std::env::var(KEY_ID_ENV)
            .map_err(|_| KalshiError::Auth(format!("{KEY_ID_ENV} is not set")))?;
        let path = std::env::var(PRIVATE_KEY_PATH_ENV)
            .map_err(|_| KalshiError::Auth(format!("{PRIVATE_KEY_PATH_ENV} is not set")))?;
        let pem = std::fs::read_to_string(&path).map_err(|error| {
            KalshiError::Auth(format!("cannot read private key file: {}", error.kind()))
        })?;
        Self::from_pem(key_id, &pem)
    }

    /// Signs `timestamp_ms + method + path` with RSA-PSS/SHA-256 (salt length = digest length).
    ///
    /// `path` is the full URL path from the host root (for example `/trade-api/v2/portfolio/balance`)
    /// and must not include a query string; any query string is stripped defensively.
    pub fn sign(&self, timestamp_ms: u128, method: &str, path: &str) -> SignedHeaders {
        let path = path.split('?').next().unwrap_or(path);
        let timestamp = timestamp_ms.to_string();
        let message = format!("{timestamp}{method}{path}");
        let signature = self
            .signing_key
            .sign_with_rng(&mut rand::thread_rng(), message.as_bytes());
        SignedHeaders {
            key_id: self.key_id.clone(),
            timestamp,
            signature: STANDARD.encode(signature.to_bytes()),
        }
    }

    /// Signs a request using the current wall-clock time.
    pub(crate) fn sign_now(&self, method: &str, path: &str) -> SignedHeaders {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        self.sign(now, method, path)
    }
}
