#![cfg(feature = "trading")]

mod common;

use common::demo::{demo_credentials, demo_url};
use furball::{
    AmendOrder, Client, MarketFilter, MarketStatus, OrderBuilder, OrderSide, Price,
    SelfTradePrevention, TimeInForce, TradingClient, WriteOutcome,
};
use rust_decimal::Decimal;

async fn open_ticker(client: &Client) -> String {
    let filter = MarketFilter {
        status: Some(MarketStatus::Open),
        ..MarketFilter::default()
    };
    let markets = client.markets(&filter, 1, 1).await.expect("markets");
    markets
        .into_iter()
        .next()
        .expect("an open demo market")
        .ticker
}

fn sent<T>(outcome: WriteOutcome<T>) -> T {
    match outcome {
        WriteOutcome::Sent(value) => value,
        WriteOutcome::DryRun(_) => panic!("write was not sent"),
    }
}

#[tokio::test]
#[ignore = "network: places, amends and cancels an order on the Kalshi demo environment"]
async fn places_amends_and_cancels_a_demo_order() {
    let Some(credentials) = demo_credentials() else {
        return;
    };
    let base_url = demo_url();
    let reader = Client::builder()
        .base_url(&base_url)
        .build()
        .expect("reader");
    let ticker = open_ticker(&reader).await;
    let trader = TradingClient::builder()
        .base_url(&base_url)
        .credentials(credentials)
        .build()
        .expect("trading client");

    let low = Price::new("0.0100".parse().expect("price")).expect("price");
    let order = OrderBuilder::new(
        ticker.clone(),
        OrderSide::Bid,
        1.into(),
        low,
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::Maker,
    )
    .post_only(true)
    .build();
    let placed = sent(trader.create_order(&order).await.expect("create"));
    assert_eq!(placed.fill_count, Decimal::ZERO);

    let amend = AmendOrder {
        ticker,
        side: OrderSide::Bid,
        price: "0.0200".parse().expect("price"),
        count: Decimal::ONE,
        client_order_id: placed.client_order_id.clone(),
        updated_client_order_id: None,
        expiration_time: None,
    };
    let amended = sent(
        trader
            .amend_order(&placed.order_id, &amend)
            .await
            .expect("amend"),
    );

    let cancelled = sent(
        trader
            .cancel_order(&amended.order_id)
            .await
            .expect("cancel"),
    );
    assert_eq!(cancelled.reduced_by, Decimal::ONE);
}
