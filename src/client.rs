use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use reqwest::header::RETRY_AFTER;
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use crate::auth::{
    ACCESS_KEY_HEADER, ACCESS_SIGNATURE_HEADER, ACCESS_TIMESTAMP_HEADER, Credentials,
};
use crate::error::{ApiError, KalshiError, Result};
use crate::rate_limit::{DEFAULT_COST, ReadBucket, Tier};
use crate::routing::Cutoff;

/// Production base URL.
pub const PRODUCTION_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
/// Alternate production base URL on the dedicated external API host.
pub const PRODUCTION_EXTERNAL_URL: &str = "https://external-api.kalshi.com/trade-api/v2";
/// Demo base URL.
pub const DEMO_URL: &str = "https://demo-api.kalshi.co/trade-api/v2";

/// Maximum attempts for one request, counting the first.
pub const MAX_ATTEMPTS: u32 = 5;

/// One Kalshi API request: method, where it goes, what it costs, whether it must be signed,
/// and whether it is safe to retry.
#[derive(Debug, Clone)]
pub struct Request {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) body: Option<serde_json::Value>,
    pub(crate) cost: u32,
    pub(crate) authenticated: bool,
    pub(crate) idempotent: bool,
}

impl Request {
    /// A GET read costing the default 10 tokens. `path` is relative to the base URL and starts with `/`.
    /// Idempotent: a failure after dispatch surfaces as [`KalshiError::Transport`].
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path, true)
    }

    /// A POST write costing the default 10 tokens. Not idempotent by default: a timeout,
    /// connection failure, or HTTP 5xx surfaces as [`KalshiError::Ambiguous`] because the write
    /// may have landed. Callers that make a POST safe to retry (for example by supplying a
    /// `client_order_id`) should mark it idempotent.
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path, false)
    }

    /// A PUT write costing the default 10 tokens. Idempotent by method.
    pub fn put(path: impl Into<String>) -> Self {
        Self::new(Method::PUT, path, true)
    }

    /// A DELETE write costing the default 10 tokens. Idempotent by method.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::DELETE, path, true)
    }

    fn new(method: Method, path: impl Into<String>, idempotent: bool) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            cost: DEFAULT_COST,
            authenticated: false,
            idempotent,
        }
    }

    /// Attaches a JSON request body. The body is never part of the signature.
    pub fn body(mut self, body: impl Serialize) -> Self {
        self.body =
            Some(serde_json::to_value(body).expect("request body must be JSON-serializable"));
        self
    }

    /// Adds a query parameter.
    pub fn query(mut self, key: &str, value: impl ToString) -> Self {
        self.query.push((key.to_owned(), value.to_string()));
        self
    }

    /// Adds a query parameter when `value` is present.
    pub fn query_opt(self, key: &str, value: Option<impl ToString>) -> Self {
        match value {
            Some(value) => self.query(key, value),
            None => self,
        }
    }

    /// Sets the token cost this endpoint declares.
    pub fn cost(mut self, cost: u32) -> Self {
        self.cost = cost;
        self
    }

    /// Requires signing; fails with [`KalshiError::Auth`] when the client has no credentials.
    pub fn authenticated(mut self) -> Self {
        self.authenticated = true;
        self
    }

    /// Sets whether the request is safe to retry after a timeout, connection failure, or
    /// HTTP 5xx. GET, PUT and DELETE default to `true`; POST defaults to `false`.
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }
}

/// Builder for [`Client`].
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    base_url: String,
    tier: Tier,
    credentials: Option<Credentials>,
    backoff_base: Duration,
    cutoff_ttl: Duration,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            base_url: PRODUCTION_URL.to_owned(),
            tier: Tier::default(),
            credentials: None,
            backoff_base: Duration::from_millis(250),
            cutoff_ttl: Duration::from_secs(600),
        }
    }
}

impl ClientBuilder {
    /// Overrides the base URL (for example [`DEMO_URL`] or a test server).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the account tier that sizes the rate limiter. Defaults to [`Tier::Basic`].
    pub fn tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }

    /// Attaches credentials used for requests marked [`Request::authenticated`].
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Sets the first 429 retry delay; each further retry doubles it, plus jitter.
    pub fn backoff_base(mut self, backoff_base: Duration) -> Self {
        self.backoff_base = backoff_base;
        self
    }

    /// Sets how long the historical cutoff is cached. Defaults to 10 minutes.
    pub fn cutoff_ttl(mut self, cutoff_ttl: Duration) -> Self {
        self.cutoff_ttl = cutoff_ttl;
        self
    }

    /// Validates the base URL and builds the client.
    pub fn build(self) -> Result<Client> {
        let base = Url::parse(&self.base_url)
            .map_err(|error| KalshiError::InvalidUrl(error.to_string()))?;
        if base.cannot_be_a_base() {
            return Err(KalshiError::InvalidUrl("base url cannot be a base".into()));
        }
        Ok(Client {
            inner: Arc::new(Inner {
                http: reqwest::Client::new(),
                base,
                credentials: self.credentials,
                bucket: ReadBucket::new(self.tier.read_budget(), self.tier.read_capacity()),
                backoff_base: self.backoff_base,
                cutoff_ttl: self.cutoff_ttl,
                cutoff: Mutex::new(None),
            }),
        })
    }
}

pub(crate) struct Inner {
    http: reqwest::Client,
    base: Url,
    credentials: Option<Credentials>,
    bucket: ReadBucket,
    backoff_base: Duration,
    pub(crate) cutoff_ttl: Duration,
    pub(crate) cutoff: Mutex<Option<(Instant, Cutoff)>>,
}

/// Read-only Kalshi Trade API client. Cheap to clone; clones share the rate limiter and cutoff cache.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

impl Client {
    /// Starts a [`ClientBuilder`] targeting production with the Basic tier and no credentials.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Sends `request` and decodes the JSON body as `T`.
    pub async fn send<T: DeserializeOwned>(&self, request: &Request) -> Result<T> {
        let body = self.send_text(request).await?;
        serde_json::from_str(&body).map_err(|source| KalshiError::Decode { body, source })
    }

    pub(crate) async fn send_text(&self, request: &Request) -> Result<String> {
        let inner = &self.inner;
        let mut attempt = 1;
        loop {
            inner.bucket.acquire(request.cost).await;
            let response = match self.build(request)?.send().await {
                Ok(response) => response,
                Err(error) => return Err(transport_failure(request, error)),
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                if attempt >= MAX_ATTEMPTS {
                    return Err(KalshiError::RateLimited);
                }
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let delay = retry_after.unwrap_or_else(|| self.backoff(attempt));
                tracing::warn!(attempt, ?delay, path = %request.path, "kalshi rate limited; backing off");
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => return Err(transport_failure(request, error)),
            };
            return match status {
                status if status.is_success() => Ok(body),
                StatusCode::NOT_FOUND => Err(KalshiError::NotFound),
                status if status.is_server_error() && !request.idempotent => {
                    Err(KalshiError::Ambiguous(format!(
                        "Kalshi answered HTTP {} ({}); the write may have landed",
                        status.as_u16(),
                        error_detail(&body)
                    )))
                }
                status => Err(decode_error(status, body)),
            };
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let exponential = self.inner.backoff_base * 2u32.saturating_pow(attempt - 1);
        let jitter = rand::thread_rng().gen_range(0.0..=0.5);
        exponential.mul_f64(1.0 + jitter)
    }

    fn build(&self, request: &Request) -> Result<reqwest::RequestBuilder> {
        let inner = &self.inner;
        let base = inner.base.as_str().trim_end_matches('/');
        let url = format!("{base}{}", request.path);
        let mut builder = inner
            .http
            .request(request.method.clone(), url)
            .query(&request.query);
        if let Some(body) = &request.body {
            builder = builder.json(body);
        }
        if request.authenticated {
            let credentials = inner.credentials.as_ref().ok_or_else(|| {
                KalshiError::Auth("endpoint requires credentials but none were configured".into())
            })?;
            let sign_path = format!(
                "{}{}",
                inner.base.path().trim_end_matches('/'),
                request.path
            );
            let headers = credentials.sign_now(request.method.as_str(), &sign_path);
            builder = builder
                .header(ACCESS_KEY_HEADER, headers.key_id)
                .header(ACCESS_TIMESTAMP_HEADER, headers.timestamp)
                .header(ACCESS_SIGNATURE_HEADER, headers.signature);
        }
        Ok(builder)
    }
}

/// A request that failed without an HTTP response: for idempotent requests that is an ordinary
/// [`KalshiError::Transport`], for non-idempotent ones the outcome is unknown.
fn transport_failure(request: &Request, error: reqwest::Error) -> KalshiError {
    if request.idempotent {
        KalshiError::from(error)
    } else if error.is_timeout() {
        KalshiError::Ambiguous("request timed out; the write may have landed".into())
    } else {
        KalshiError::Ambiguous("connection to Kalshi failed; the write may have landed".into())
    }
}

/// Decodes a Kalshi error response: its JSON envelope when present, raw text otherwise.
fn decode_error(status: StatusCode, body: String) -> KalshiError {
    match serde_json::from_str::<ApiError>(&body) {
        Ok(error) => KalshiError::Api {
            status,
            code: error.code,
            message: error.message,
        },
        Err(_) => KalshiError::Http { status, body },
    }
}

/// Renders an error body as `code: message` when it is a Kalshi JSON envelope, else raw text.
fn error_detail(body: &str) -> String {
    serde_json::from_str::<ApiError>(body)
        .map(|error| format!("code {}: {}", error.code, error.message))
        .unwrap_or_else(|_| body.to_owned())
}
