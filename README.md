# furball

A Rust SDK for [Kalshi's Trade API v2](https://docs.kalshi.com). Read-only by default; the trading
surface is opt-in behind the `trading` cargo feature.

- Request signing (Ed25519 or RSA-PSS/SHA-256, detected from the PEM key) for authenticated endpoints; public endpoints need no keys.
- Token-bucket rate limiting sized by account tier, with retry on HTTP 429 (exponential backoff, jitter, `Retry-After`).
- Independent Read, Write and balance token buckets per [`Tier`](https://docs.kalshi.com/getting_started/rate_limits); a request costing more than its bucket holds is rejected locally (`KalshiError::CostExceedsBucket`) before it is sent.
- `Client::account_limits` and `Client::account_endpoint_costs` read the account's usage tier, bucket limits, grants, and non-default endpoint costs.
- GET, POST, PUT and DELETE transport with JSON bodies. A non-idempotent write that fails after dispatch (timeout, connection failure, HTTP 5xx) yields `KalshiError::Ambiguous` instead of being retried.
- Cursor pagination as a lazy `Stream`.
- Transparent routing between live and historical endpoints (markets, candlesticks, trades) using the cached `/historical/cutoff`.
- Authenticated portfolio reads: balance (its own per-second bucket), positions, settlements, subaccount balances.
- Authenticated order and fill reads: one order (2 tokens), order lists, fills, queue positions, and complete position history; live/archive split at the cutoff, merged like trades.
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

## Trading (opt-in)

The write surface lives on `TradingClient`, behind the `trading` cargo feature (off by default, so
a default build contains no write methods):

```rust,ignore
let client = furball::TradingClient::builder()
    .credentials(furball::Credentials::from_env()?)
    .build()?; // demo environment by default
```

- Production requires both `Environment::Production` **and** `allow_real_money()`; otherwise `build()` fails.
- `TradingClient::halt()` is a process-global kill switch: every later write fails with `KalshiError::TradingHalted` without a network call (`TradingClient::resume()` releases it).
- `dry_run(true)` validates and signs each write but returns `WriteOutcome::DryRun` instead of sending.
- Every write attempt is logged at `info` (method, path, ticker, client order ID), never keys or signatures.

### Safety model

```text
default build ──► Client only: no write method exists
      │
      └─ feature `trading` ──► TradingClient ──► demo URL by default
                                    │
                                    ├─ dry_run(true)        sign, never send
                                    ├─ halt()               refuse every write
                                    └─ Environment::Production + allow_real_money()
                                                            the only path to real money
```

Both production opt-ins must be explicit and in code; no environment variable switches a client to production.
Try strategies on `FakeExchange` first, then on demo, and only then consider production.

## Examples

| Example | Run | Needs |
| --- | --- | --- |
| Read markets | `cargo run --example read_markets -- KXHIGHNY` | network |
| Poll candlesticks | `cargo run --example stream_candlesticks -- <series> <market>` | network |
| Compute a fee | `cargo run --example compute_fee` | nothing |
| Place a demo order | `cargo run --example place_demo_order --features trading -- <market>` | demo credentials |
| Fake exchange | `cargo run --example fake_exchange --features testing` | nothing |

`cargo test` compiles every example whose features are enabled; CI runs it with default and all features.

## Testing (opt-in)

`furball::testing::FakeExchange` (behind the `testing` cargo feature, which enables `trading`) is an in-process fake exchange for testing strategies without Kalshi: price-and-time priority matching for limit orders (good-till-canceled, immediate-or-cancel, fill-or-kill), fills at the resting price, fees from `FeeCalculator` with a per-order accumulator, and tracked balance, positions, realized P&L and settlements. The test controls the clock, injects counterparty liquidity and market data, and settles markets. Its module docs carry a worked strategy example.

## Fees

Fee type names and rounding come from Kalshi's docs. The fee schedule builder reads fee types and multipliers from the API.

The base coefficients are not in the API. They come from Kalshi's [fee schedule](https://kalshi.com/fee-schedule): 0.07 for takers and 0.0175 for makers on most markets. On combos, the maker fee is 50% of the taker fee. The `flat` fee formula is not documented.

## Helpers

Pure helpers that make no network calls:

- Validated market quantities: `Price` (dollars, strictly between 0 and 1) and `Count` (contracts, non-negative). `Price::validate` checks a price against the market's tick size from `price_level_structure` or `price_ranges`; `Price::from_cents`/`Price::to_cents` convert between cents and dollars in `Decimal` arithmetic, never `f64`. `OrderBuilder` takes `Count` and `Price`.
- Order book helpers on `Orderbook`: best bid and ask on both sides (Kalshi returns bids only; each ask is derived as `1 -` the opposite bid), spread, mid price, depth at N levels, and `cost_to_fill(side, count)` returning the volume-weighted average price and total cost, or `BookError::TooThin` when the book cannot cover the request.
- Position math from fills, in Kalshi's netting model: the settlement payoff of yes and no positions on binary and scalar markets (`payoff_at_settlement`), realized and unrealized profit and loss with fees subtracted (`realized_pnl`, `unrealized_pnl`), and the volume-weighted average entry price (`average_entry_price`).
- Fee calculator: `FeeCalculator` computes a fill's fee exactly as Kalshi does — the quadratic formula family with the published coefficients (`FeeCoefficients::default`; overridable via `FURBALL_FEE_TAKER`/`FURBALL_FEE_MAKER`/`FURBALL_FEE_COMBO_MAKER`), the fee-rounding pipeline (trade fee rounded up to $0.000001, balance alignment to $0.01 or $0.0001, per-order rebate accumulator), and `fee_in_force` to resolve the fee type and multiplier from a fee schedule at the fill time. `flat` has no documented formula and errors. The maker coefficients are unconfirmed defaults: the fee-schedule page is behind a bot check, so a person must verify them in a browser before release.

## License

MIT

## Demo integration tests (opt-in)

Tests in `tests/demo_read.rs` and `tests/demo_trading.rs` call Kalshi's demo environment. They are
`#[ignore]`d, so a plain `cargo test` makes no network call.

```sh
cargo test --all-features -- --ignored
```

The order test (place, amend, cancel) needs demo credentials: `KALSHI_API_KEY_ID` and
`KALSHI_PRIVATE_KEY_PATH`. Without them it prints `SKIPPED` and returns. Every demo test builds its
client from `KALSHI_DEMO_URL` (default: the demo URL) and panics if that URL is a production URL
or not a demo host.
