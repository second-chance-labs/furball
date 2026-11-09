# furball

A read-only Rust SDK for [Kalshi's Trade API v2](https://docs.kalshi.com). No trading endpoints.

- Request signing (Ed25519 or RSA-PSS/SHA-256, detected from the PEM key) for authenticated endpoints; public endpoints need no keys.
- Token-bucket rate limiting sized by account tier, with retry on HTTP 429 (exponential backoff, jitter, `Retry-After`).
- Independent Read, Write and balance token buckets per [`Tier`](https://docs.kalshi.com/getting_started/rate_limits); a request costing more than its bucket holds is rejected locally (`KalshiError::CostExceedsBucket`) before it is sent.
- `Client::account_limits` and `Client::account_endpoint_costs` read the account's usage tier, bucket limits, grants, and non-default endpoint costs.
- GET, POST, PUT and DELETE transport with JSON bodies. A non-idempotent write that fails after dispatch (timeout, connection failure, HTTP 5xx) yields `KalshiError::Ambiguous` instead of being retried.
- Cursor pagination as a lazy `Stream`.
- Transparent routing between live and historical endpoints (markets, candlesticks, trades) using the cached `/historical/cutoff`.
- Typed responses; money and counts are `rust_decimal::Decimal`, never `f64`.
- Fee schedule builder: series fees and event overrides as non-overlapping intervals.

```rust,no_run
use furball::{Client, MarketFilter};

# async fn run() -> furball::Result<()> {
let client = Client::builder().build()?; // production, Basic tier, no credentials
let series = client.series("KXHIGHNY").await?;
let markets = client.markets(&MarketFilter::series("KXHIGHNY"), 100, 1_000).await?;
println!("{} fee {:?}, {} markets", series.ticker, series.fee_type, markets.len());
# Ok(())
# }
```

Authenticated endpoints (CF Benchmarks passthrough) need credentials:
`Credentials::from_env()` reads `KALSHI_API_KEY_ID` and the PEM file named by `KALSHI_PRIVATE_KEY_PATH`.
Credentials never appear in `Debug` output or error messages.

## Fees

Fee type names and rounding come from Kalshi's docs. The fee schedule builder reads fee types and multipliers from the API.

The base coefficients are not in the API. They come from Kalshi's [fee schedule](https://kalshi.com/fee-schedule): 0.07 for takers and 0.0175 for makers on most markets. On combos, the maker fee is 50% of the taker fee. The `flat` fee formula is not documented.

## License

MIT
