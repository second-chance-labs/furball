# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/); see [RELEASING.md](RELEASING.md) for what counts as a
breaking change.

## [Unreleased]

### Added

- Signed, rate-limited client for Kalshi's Trade API v2 with 429 retry, cursor pagination and
  transparent live/historical routing.
- Typed market, order, fill, position, settlement, RFQ, quote and block trade reads.
- `trading` feature: `TradingClient` writes, demo by default, `allow_real_money()` opt-in for
  production, `dry_run` and a process-global `halt()` kill switch.
- `testing` feature: in-process `FakeExchange`.
- Fee calculator, order book helpers, position math, and validated `Price` and `Count`.
- GitHub Actions CI (format, clippy, tests on stable and the minimum supported Rust version, docs)
  and a husky `pre-push` hook running the same checks.
- Opt-in demo environment integration tests (`cargo test -- --ignored`).
- Examples: read markets, poll candlesticks, compute a fee, place a demo order, fake exchange.
