//! Lists open markets of one series. Needs no credentials.
//!
//! `cargo run --example read_markets -- KXHIGHNY`

use furball::{Client, MarketFilter, MarketStatus};

#[tokio::main]
async fn main() -> furball::Result<()> {
    let series_ticker = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "KXHIGHNY".to_owned());
    let client = Client::builder().build()?;

    let series = client.series(&series_ticker).await?;
    println!("{}: fee type {:?}", series.ticker, series.fee_type);

    let filter = MarketFilter {
        status: Some(MarketStatus::Open),
        series_ticker: Some(series_ticker),
        ..MarketFilter::default()
    };
    for market in client.markets(&filter, 100, 1_000).await? {
        println!("{} closes {}", market.ticker, market.close_time);
    }
    Ok(())
}
