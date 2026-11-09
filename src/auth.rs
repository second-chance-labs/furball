use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey as Ed25519SigningKey};
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use sha2::Sha256;

use crate::error::{KalshiError, Result};

/// Environment variable holding the API key ID.
pub const KEY_ID_ENV: &str = "KALSHI_API_KEY_ID";
/// Environment variable holding the path to the PEM-encoded private key (RSA or Ed25519).
pub const PRIVATE_KEY_PATH_ENV: &str = "KALSHI_PRIVATE_KEY_PATH";

/// Header carrying the API key ID.
pub const ACCESS_KEY_HEADER: &str = "KALSHI-ACCESS-KEY";
/// Header carrying the request timestamp in milliseconds since the Unix epoch.
pub const ACCESS_TIMESTAMP_HEADER: &str = "KALSHI-ACCESS-TIMESTAMP";
/// Header carrying the base64 request signature (RSA-PSS/SHA-256 or Ed25519).
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

/// Type of the private key held by [`Credentials`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    /// RSA key, signing with RSA-PSS/SHA-256 (salt length = digest length).
    Rsa,
    /// Ed25519 key, signing the message directly.
    Ed25519,
}

#[derive(Clone)]
enum SigningKey {
    Rsa(BlindedSigningKey<Sha256>),
    Ed25519(Ed25519SigningKey),
}

/// An API key ID plus its private key (RSA or Ed25519). `Debug` never prints either.
#[derive(Clone)]
pub struct Credentials {
    key_id: String,
    signing_key: SigningKey,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

impl Credentials {
    /// Builds credentials from an API key ID and a PEM private key.
    ///
    /// The key type is detected from the key itself, since a PKCS#8 PEM does not identify it:
    /// Ed25519 PKCS#8 is tried first, then RSA PKCS#8, then RSA PKCS#1.
    pub fn from_pem(key_id: impl Into<String>, pem: &str) -> Result<Self> {
        let signing_key = match Ed25519SigningKey::from_pkcs8_pem(pem) {
            Ok(key) => SigningKey::Ed25519(key),
            Err(_) => match RsaPrivateKey::from_pkcs8_pem(pem) {
                Ok(key) => SigningKey::Rsa(BlindedSigningKey::<Sha256>::new(key)),
                Err(_) => {
                    let key = RsaPrivateKey::from_pkcs1_pem(pem).map_err(|_| {
                        KalshiError::Auth("private key is not a valid RSA or Ed25519 PEM".into())
                    })?;
                    SigningKey::Rsa(BlindedSigningKey::<Sha256>::new(key))
                }
            },
        };
        Ok(Self {
            key_id: key_id.into(),
            signing_key,
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

    /// Type of the private key these credentials hold.
    pub fn key_type(&self) -> KeyType {
        match self.signing_key {
            SigningKey::Rsa(_) => KeyType::Rsa,
            SigningKey::Ed25519(_) => KeyType::Ed25519,
        }
    }

    /// Signs `timestamp_ms + method + path`: RSA-PSS/SHA-256 (salt length = digest length) for
    /// RSA keys, plain Ed25519 for Ed25519 keys; the signature is base64-encoded.
    ///
    /// `path` is the full URL path from the host root (for example `/trade-api/v2/portfolio/balance`)
    /// and must not include a query string; any query string is stripped defensively.
    pub fn sign(&self, timestamp_ms: u128, method: &str, path: &str) -> SignedHeaders {
        let path = path.split('?').next().unwrap_or(path);
        let timestamp = timestamp_ms.to_string();
        let message = format!("{timestamp}{method}{path}");
        let signature = match &self.signing_key {
            SigningKey::Rsa(key) => STANDARD.encode(
                key.sign_with_rng(&mut rand::thread_rng(), message.as_bytes())
                    .to_bytes(),
            ),
            SigningKey::Ed25519(key) => STANDARD.encode(key.sign(message.as_bytes()).to_bytes()),
        };
        SignedHeaders {
            key_id: self.key_id.clone(),
            timestamp,
            signature,
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
