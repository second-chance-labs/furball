//! Tests a strategy offline with the in-process fake exchange: buy at 0.55, sell at 0.65.
//!
//! `cargo run --example fake_exchange --features testing`

use furball::testing::FakeExchange;
use furball::{Count, OrderBuilder, OrderSide, SelfTradePrevention, TimeInForce};
use rust_decimal::Decimal;

fn limit_order(side: OrderSide, price: &str, time_in_force: TimeInForce) -> furball::CreateOrderV2 {
    OrderBuilder::new(
        "KXTEST",
        side,
        Count::from(100),
        price.parse().expect("price"),
        time_in_force,
        SelfTradePrevention::TakerAtCross,
    )
    .build()
}

fn main() {
    let exchange = FakeExchange::new(Decimal::new(1_000, 0));
    exchange.add_market("KXTEST");

    exchange
        .add_liquidity(
            "KXTEST",
            OrderSide::Ask,
            "0.55".parse().expect("price"),
            Count::from(100),
        )
        .expect("market exists");
    exchange
        .create_order(&limit_order(
            OrderSide::Bid,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .expect("buy");

    exchange
        .add_liquidity(
            "KXTEST",
            OrderSide::Bid,
            "0.65".parse().expect("price"),
            Count::from(100),
        )
        .expect("market exists");
    exchange
        .create_order(&limit_order(
            OrderSide::Ask,
            "0.65",
            TimeInForce::ImmediateOrCancel,
        ))
        .expect("sell");

    println!("balance {}", exchange.balance());
    for position in exchange.positions() {
        println!(
            "{} realized {}",
            position.ticker, position.realized_pnl_dollars
        );
    }
}
