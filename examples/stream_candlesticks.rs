//! Prints hourly candlesticks of one market as a lazily polled feed: every minute it asks for
//! the candles since the last one it printed. Needs no credentials.
//!
//! `cargo run --example stream_candlesticks -- KXHIGHNY KXHIGHNY-26NOV12-T60`

use std::time::Duration;

use chrono::Utc;
use furball::{Client, Period};

#[tokio::main]
async fn main() -> furball::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(series_ticker), Some(ticker)) = (args.next(), args.next()) else {
        eprintln!("usage: stream_candlesticks <series_ticker> <market_ticker>");
        return Ok(());
    };
    let client = Client::builder().build()?;

    let mut since = Utc::now().timestamp() - 24 * 3_600;
    loop {
        let now = Utc::now().timestamp();
        let candles = client
            .get_candlesticks(&series_ticker, &ticker, since, now, Period::Hour)
            .await?;
        let printed_through = since;
        for candle in candles
            .iter()
            .filter(|candle| candle.end_period_ts > printed_through)
        {
            println!(
                "{} close {:?} volume {}",
                candle.end_period_ts, candle.price.close_dollars, candle.volume_fp
            );
            since = candle.end_period_ts;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
