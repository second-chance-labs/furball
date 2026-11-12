mod common;

use common::demo::demo_url;
use furball::{Client, MarketFilter, MarketStatus};

fn client() -> Client {
    Client::builder()
        .base_url(demo_url())
        .build()
        .expect("client")
}

#[tokio::test]
#[ignore = "network: reads from the Kalshi demo environment"]
async fn lists_open_markets_and_reads_one_with_its_orderbook() {
    let client = client();
    let filter = MarketFilter {
        status: Some(MarketStatus::Open),
        ..MarketFilter::default()
    };
    let markets = client.markets(&filter, 10, 10).await.expect("markets");
    let listed = markets.first().expect("demo has at least one open market");

    let market = client.get_market(&listed.ticker).await.expect("market");
    assert_eq!(market.ticker, listed.ticker);

    let book = client
        .orderbook(&listed.ticker, 5)
        .await
        .expect("orderbook");
    assert!(book.yes_dollars.len() <= 5);
    assert!(book.no_dollars.len() <= 5);
}

#[tokio::test]
#[ignore = "network: reads from the Kalshi demo environment"]
async fn reads_the_historical_cutoff() {
    client().cutoff().await.expect("cutoff");
}
