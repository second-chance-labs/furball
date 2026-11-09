use reqwest::StatusCode;
use serde::Deserialize;

/// Kalshi's JSON error envelope, sent with most error responses.
#[derive(Deserialize)]
pub(crate) struct ApiError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

/// Failure modes of a Kalshi request. No variant ever carries credentials or auth headers.
#[derive(Debug, thiserror::Error)]
pub enum KalshiError {
    /// Kalshi answered with non-JSON error text at this status.
    #[error("kalshi returned HTTP {status}: {body}")]
    Http {
        /// Response status.
        status: StatusCode,
        /// Raw response body.
        body: String,
    },
    /// Kalshi answered with its JSON error envelope at this status.
    #[error("kalshi returned HTTP {status} (code {code}): {message}")]
    Api {
        /// Response status.
        status: StatusCode,
        /// Kalshi's numeric error code.
        code: i64,
        /// Kalshi's error message.
        message: String,
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
    /// A non-idempotent request failed after it may have reached Kalshi: a timeout, a
    /// connection failure, or HTTP 5xx. It may have landed; reconcile (for example by
    /// querying the orders it could have created) before retrying.
    #[error("kalshi write outcome is ambiguous: {0}")]
    Ambiguous(String),
    /// The request's declared cost exceeds the capacity of the rate bucket it draws from.
    /// It is rejected locally and never sent.
    #[error("request cost {cost} exceeds the rate bucket capacity {capacity}")]
    CostExceedsBucket {
        /// Declared request cost.
        cost: u32,
        /// Capacity of the bucket the request draws from.
        capacity: u32,
    },
    /// A trading write was attempted while the global kill switch is on. No network call was made.
    #[error("trading is halted")]
    TradingHalted,
    /// A `TradingClient` was misconfigured and refuses to build.
    #[error("trading client is disabled: {0}")]
    TradingDisabled(String),
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
