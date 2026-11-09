use furball::types::{BookSide, Orderbook, OrderbookResponse, PriceLevel};
use furball::{BookError, Count};
use rust_decimal::Decimal;

fn level(price: &str, count: &str) -> PriceLevel {
    let price: Decimal = price.parse().expect("decimal");
    let count: Decimal = count.parse().expect("decimal");
    PriceLevel::from((price, count))
}

fn count_of(contracts: i64) -> Count {
    Count::new(Decimal::new(contracts, 0)).expect("count")
}

#[test]
fn fixture_book_reports_derived_quotes_and_depth() {
    let file = format!(
        "{}/tests/fixtures/orderbook.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let body = std::fs::read_to_string(&file).expect("read fixture");
    let decoded: OrderbookResponse = serde_json::from_str(&body).expect("decode fixture");
    let book = decoded.orderbook_fp;

    assert_eq!(book.best_yes_bid(), None);
    assert_eq!(book.best_yes_ask(), Some("0.01".parse().expect("d")));
    assert_eq!(book.best_no_bid(), Some("0.99".parse().expect("d")));
    assert_eq!(book.best_no_ask(), None);
    assert_eq!(book.yes_spread(), None);
    assert_eq!(book.no_spread(), None);
    assert_eq!(book.yes_mid(), None);
    assert_eq!(book.no_mid(), None);

    assert_eq!(book.depth(BookSide::Bid, 3), Decimal::new(0, 0));
    assert_eq!(book.depth(BookSide::Ask, 2), "1713".parse().expect("d"));
    assert_eq!(book.depth(BookSide::Ask, 10), "1813".parse().expect("d"));

    let fill = book
        .cost_to_fill(BookSide::Bid, count_of(300))
        .expect("300 yes contracts fit in the no book");
    assert_eq!(fill.count, Decimal::new(300, 0));
    assert_eq!(fill.average_price, "0.01".parse().expect("d"));
    assert_eq!(fill.total_cost, "3.00".parse().expect("d"));

    match book.cost_to_fill(BookSide::Bid, count_of(2000)) {
        Err(BookError::TooThin {
            side,
            available,
            requested,
        }) => {
            assert_eq!(side, BookSide::Bid);
            assert_eq!(available, "1813".parse().expect("d"));
            assert_eq!(requested, Decimal::new(2000, 0));
        }
        other => panic!("expected TooThin, got {other:?}"),
    }
    match book.cost_to_fill(BookSide::Ask, count_of(1)) {
        Err(BookError::TooThin { available, .. }) => {
            assert_eq!(available, Decimal::new(0, 0));
        }
        other => panic!("expected TooThin on the empty yes book, got {other:?}"),
    }
}

#[test]
fn both_sides_hand_computed() {
    let book = Orderbook {
        yes_dollars: vec![level("0.5000", "100"), level("0.5100", "50")],
        no_dollars: vec![level("0.4800", "200"), level("0.4900", "75")],
    };

    assert_eq!(book.best_yes_bid(), Some("0.51".parse().expect("d")));
    assert_eq!(book.best_yes_ask(), Some("0.51".parse().expect("d")));
    assert_eq!(book.best_no_bid(), Some("0.49".parse().expect("d")));
    assert_eq!(book.best_no_ask(), Some("0.49".parse().expect("d")));
    assert_eq!(book.yes_spread(), Some(Decimal::new(0, 0)));
    assert_eq!(book.no_spread(), Some(Decimal::new(0, 0)));
    assert_eq!(book.yes_mid(), Some("0.51".parse().expect("d")));
    assert_eq!(book.no_mid(), Some("0.49".parse().expect("d")));
    assert_eq!(book.depth(BookSide::Bid, 1), Decimal::new(50, 0));
    assert_eq!(book.depth(BookSide::Bid, 10), "150".parse().expect("d"));

    // Buy 100 yes: 75 at 1 - 0.49 = 0.51 plus 25 at 1 - 0.48 = 0.52
    // = 38.25 + 13.00 = 51.25, average 0.5125.
    let fill = book
        .cost_to_fill(BookSide::Bid, count_of(100))
        .expect("spans two no levels");
    assert_eq!(fill.total_cost, "51.25".parse().expect("d"));
    assert_eq!(fill.average_price, "0.5125".parse().expect("d"));

    // Buy 50 no: all at 1 - 0.51 = 0.49 = 24.50.
    let fill = book
        .cost_to_fill(BookSide::Ask, count_of(50))
        .expect("single yes level");
    assert_eq!(fill.total_cost, "24.50".parse().expect("d"));
    assert_eq!(fill.average_price, "0.49".parse().expect("d"));

    assert!(matches!(
        book.cost_to_fill(BookSide::Ask, count_of(151)),
        Err(BookError::TooThin { .. })
    ));
    assert!(matches!(
        book.cost_to_fill(
            BookSide::Bid,
            Count::new(Decimal::new(0, 0)).expect("count")
        ),
        Err(BookError::NonPositiveCount(_))
    ));
}

#[test]
fn helpers_do_not_assume_book_ordering() {
    let book = Orderbook {
        yes_dollars: vec![level("0.5100", "50"), level("0.5000", "100")],
        no_dollars: vec![
            level("0.9900", "1462"),
            level("0.9800", "251"),
            level("0.9700", "100"),
        ],
    };
    assert_eq!(book.best_yes_bid(), Some("0.51".parse().expect("d")));
    assert_eq!(book.best_yes_ask(), Some("0.01".parse().expect("d")));
    assert_eq!(book.depth(BookSide::Ask, 2), "1713".parse().expect("d"));
    let fill = book
        .cost_to_fill(BookSide::Bid, count_of(300))
        .expect("fill");
    assert_eq!(fill.total_cost, "3.00".parse().expect("d"));
    assert_eq!(fill.average_price, "0.01".parse().expect("d"));
}

#[test]
fn empty_book_has_no_quotes_and_no_depth() {
    let book = Orderbook {
        yes_dollars: Vec::new(),
        no_dollars: Vec::new(),
    };
    assert_eq!(book.best_yes_bid(), None);
    assert_eq!(book.best_yes_ask(), None);
    assert_eq!(book.best_no_bid(), None);
    assert_eq!(book.best_no_ask(), None);
    assert_eq!(book.yes_spread(), None);
    assert_eq!(book.no_spread(), None);
    assert_eq!(book.yes_mid(), None);
    assert_eq!(book.no_mid(), None);
    assert_eq!(book.depth(BookSide::Bid, 5), Decimal::new(0, 0));
    assert_eq!(book.depth(BookSide::Ask, 5), Decimal::new(0, 0));
    assert!(matches!(
        book.cost_to_fill(BookSide::Bid, count_of(1)),
        Err(BookError::TooThin { available, .. }) if available == Decimal::new(0, 0)
    ));
}
