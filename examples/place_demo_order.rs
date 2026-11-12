//! Places, then cancels, a resting one-contract bid on the demo exchange. Never touches
//! production: `TradingClient` targets demo unless `Environment::Production` and
//! `allow_real_money()` are both set.
//!
//! Needs `KALSHI_API_KEY_ID` and `KALSHI_PRIVATE_KEY_PATH` for a demo account.
//!
//! `cargo run --example place_demo_order --features trading -- <open-demo-market-ticker>`

use furball::{
    Credentials, OrderBuilder, OrderSide, Price, SelfTradePrevention, TimeInForce, TradingClient,
    WriteOutcome,
};

#[tokio::main]
async fn main() -> furball::Result<()> {
    let Some(ticker) = std::env::args().nth(1) else {
        eprintln!("usage: place_demo_order <market_ticker>");
        return Ok(());
    };
    let client = TradingClient::builder()
        .credentials(Credentials::from_env()?)
        .build()?;

    let price = Price::new("0.01".parse().expect("price")).expect("price in (0, 1)");
    let order = OrderBuilder::new(
        ticker,
        OrderSide::Bid,
        1.into(),
        price,
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::Maker,
    )
    .post_only(true)
    .build();

    let WriteOutcome::Sent(placed) = client.create_order(&order).await? else {
        return Ok(());
    };
    println!(
        "placed {}, {} resting",
        placed.order_id, placed.remaining_count
    );

    if let WriteOutcome::Sent(cancelled) = client.cancel_order(&placed.order_id).await? {
        println!("cancelled, removed {} contracts", cancelled.reduced_by);
    }
    Ok(())
}
