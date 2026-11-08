use reqwest::StatusCode;

/// Failure modes of a Kalshi request. No variant ever carries credentials or auth headers.
#[derive(Debug, thiserror::Error)]
pub enum KalshiError {
    /// Kalshi answered with a non-success status other than 404 and 429.
    #[error("kalshi returned HTTP {status}: {body}")]
    Http {
        /// Response status.
        status: StatusCode,
        /// Raw response body.
        body: String,
    },
    /// HTTP 429 persisted after every retry.
    #[error("kalshi rate limit exceeded after all retries")]
    RateLimited,
    /// Credentials are missing, unreadable, malformed, or the request could not be signed.
    #[error("kalshi authentication failed: {0}")]
    Auth(String),
    /// A response body did not match the expected shape.
    #[error("failed to decode kalshi response: {source}; body: {body}")]
    Decode {
        /// Raw response body.
        body: String,
        /// Underlying deserialization error.
        source: serde_json::Error,
    },
    /// HTTP 404.
    #[error("kalshi resource not found")]
    NotFound,
    /// The request never produced an HTTP response (connection, TLS, timeout).
    #[error("kalshi transport error: {0}")]
    Transport(reqwest::Error),
    /// The configured base URL or a request path is not a valid URL.
    #[error("invalid kalshi url: {0}")]
    InvalidUrl(String),
}

impl From<reqwest::Error> for KalshiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, KalshiError>;
