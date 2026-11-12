#![cfg(feature = "testing")]

use chrono::{DateTime, Utc};
use furball::testing::{ExchangeError, FakeExchange};
use furball::types::{MarketResult, OrderStatus};
use furball::{
    Count, CreateOrderV2, OrderBuilder, OrderSide, Price, SelfTradePrevention, TimeInForce,
};
use rust_decimal::Decimal;

fn d(text: &str) -> Decimal {
    text.parse().expect("decimal")
}

fn price(text: &str) -> Price {
    text.parse().expect("price")
}

fn limit_order(
    ticker: &str,
    side: OrderSide,
    count: u32,
    price: &str,
    time_in_force: TimeInForce,
) -> CreateOrderV2 {
    OrderBuilder::new(
        ticker,
        side,
        Count::from(count),
        price.parse().expect("price"),
        time_in_force,
        SelfTradePrevention::TakerAtCross,
    )
    .client_order_id(format!("{side:?}-{price}"))
    .build()
}

fn ts(text: &str) -> DateTime<Utc> {
    text.parse().expect("timestamp")
}

#[test]
fn matching_executes_at_the_resting_price() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();

    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.60",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("100"));
    assert_eq!(order.remaining_count, d("0"));
    // the fill executes at the resting ask, not the taker's limit
    let fill = exchange.fills(None).pop().expect("taker fill");
    assert_eq!(fill.yes_price_dollars, d("0.55"));
    assert!(fill.is_taker);
    // fee: 0.07 * 0.55 * 0.45 * 100 = 1.7325, revenue -55 -> net 1.74
    assert_eq!(fill.fee_cost, d("1.74"));
    assert_eq!(exchange.balance(), d("943.26"));
    let position = exchange.positions().pop().expect("position");
    assert_eq!(position.position_fp, d("100"));
    assert_eq!(position.market_exposure_dollars, d("55"));
}

#[test]
fn partial_fill_leaves_the_remainder_resting() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Bid, price("0.50"), Count::from(40))
        .unwrap();

    // the ask fills what the book holds and rests the rest
    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.45",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("40"));
    assert_eq!(order.remaining_count, d("60"));
    // filled at the resting bid 0.50, fee 0.07 * 0.5 * 0.5 * 40 = 0.70
    let fill = exchange.fills(None).pop().expect("taker fill");
    assert_eq!(fill.yes_price_dollars, d("0.50"));
    assert_eq!(fill.fee_cost, d("0.70"));
    assert_eq!(exchange.balance(), d("1019.30"));
    assert_eq!(
        exchange.positions().pop().expect("position").position_fp,
        d("-40")
    );

    let book = exchange.order_book("KXTEST").unwrap();
    assert!(book.yes_dollars.is_empty());
    assert_eq!(book.no_dollars.len(), 1);
    assert_eq!(book.no_dollars[0].price_dollars, d("0.55"));
    assert_eq!(book.no_dollars[0].count_fp, d("60"));

    // cancel the remainder
    let canceled = exchange.cancel_order(&order.order_id).unwrap();
    assert_eq!(canceled.reduced_by, d("60"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Canceled
    );
    assert!(exchange.order_book("KXTEST").unwrap().no_dollars.is_empty());
}

#[test]
fn price_priority_fills_the_best_price_first() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.50"), Count::from(100))
        .unwrap();

    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            150,
            "0.60",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("150"));

    let fills = exchange.fills(None);
    assert_eq!(fills.len(), 2);
    // 100 at 0.50, then 50 at 0.55
    assert_eq!(fills[0].count_fp, d("100"));
    assert_eq!(fills[0].yes_price_dollars, d("0.50"));
    assert_eq!(fills[1].count_fp, d("50"));
    assert_eq!(fills[1].yes_price_dollars, d("0.55"));
    // cost 77.50, fees 1.75 + 0.87
    assert_eq!(exchange.balance(), d("919.88"));
    assert_eq!(
        exchange.positions().pop().expect("position").position_fp,
        d("150")
    );
}

#[test]
fn time_priority_fills_the_oldest_order_first_within_a_price() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    let first = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    let second = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();

    // the taker takes the first resting ask in full, then half of the second
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            150,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(
        exchange.order(&first.order_id).unwrap().status,
        OrderStatus::Executed
    );
    assert_eq!(
        exchange.order(&first.order_id).unwrap().fill_count_fp,
        d("100")
    );
    let second = exchange.order(&second.order_id).unwrap();
    assert_eq!(second.fill_count_fp, d("50"));
    assert_eq!(second.remaining_count_fp, d("50"));
    assert_eq!(second.status, OrderStatus::Resting);
    // the self-trades net the position back to zero
    assert_eq!(exchange.positions()[0].position_fp, d("0"));
}

#[test]
fn immediate_or_cancel_cancels_the_unfilled_remainder() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Bid, price("0.50"), Count::from(40))
        .unwrap();

    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.45",
            TimeInForce::ImmediateOrCancel,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("40"));
    assert_eq!(order.remaining_count, d("60"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Canceled
    );
    let book = exchange.order_book("KXTEST").unwrap();
    assert!(book.yes_dollars.is_empty());
    assert!(book.no_dollars.is_empty());
}

#[test]
fn fill_or_kill_rejects_when_the_book_is_too_thin() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(50))
        .unwrap();

    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.60",
            TimeInForce::FillOrKill,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("0"));
    assert_eq!(order.remaining_count, d("100"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Canceled
    );
    // nothing filled, so nothing was charged
    assert_eq!(exchange.balance(), d("1000"));
    assert!(exchange.fills(None).is_empty());

    // a fill-or-kill the book can cover executes in full
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();
    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.60",
            TimeInForce::FillOrKill,
        ))
        .unwrap();
    assert_eq!(order.fill_count, d("100"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Executed
    );
}

#[test]
fn cancel_releases_the_reserved_cash_and_rejects_bad_orders() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.50",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(exchange.balance(), d("950"));

    let canceled = exchange.cancel_order(&order.order_id).unwrap();
    assert_eq!(canceled.reduced_by, d("100"));
    assert_eq!(exchange.balance(), d("1000"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Canceled
    );

    match exchange.cancel_order(&order.order_id) {
        Err(ExchangeError::NotResting { .. }) => {}
        other => panic!("expected NotResting, got {other:?}"),
    }
    match exchange.cancel_order("nope") {
        Err(ExchangeError::UnknownOrder { .. }) => {}
        other => panic!("expected UnknownOrder, got {other:?}"),
    }

    // cancel_all clears every resting order
    let resting = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            10,
            "0.50",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(exchange.cancel_all(), 1);
    assert_eq!(
        exchange.order(&resting.order_id).unwrap().status,
        OrderStatus::Canceled
    );
}

#[test]
fn settlement_pays_the_net_position() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(exchange.balance(), d("943.26"));

    let settlement = exchange.settle("KXTEST", d("1.00")).unwrap();
    assert_eq!(settlement.market_result, MarketResult::Yes);
    assert_eq!(settlement.revenue, 10_000);
    assert_eq!(settlement.yes_count_fp, d("100"));
    assert_eq!(settlement.value, Some(100));
    assert_eq!(exchange.balance(), d("1043.26"));
    assert_eq!(exchange.positions()[0].position_fp, d("0"));

    // settling again is an error, as is trading a settled market
    match exchange.settle("KXTEST", d("1.00")) {
        Err(ExchangeError::AlreadySettled { .. }) => {}
        other => panic!("expected AlreadySettled, got {other:?}"),
    }
    match exchange.create_order(&limit_order(
        "KXTEST",
        OrderSide::Bid,
        1,
        "0.50",
        TimeInForce::GoodTillCanceled,
    )) {
        Err(ExchangeError::AlreadySettled { .. }) => {}
        other => panic!("expected AlreadySettled, got {other:?}"),
    }
}

#[test]
fn settlement_of_a_no_position_and_scalar_values() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXNO");
    exchange
        .add_liquidity("KXNO", OrderSide::Bid, price("0.60"), Count::from(100))
        .unwrap();
    // selling 100 yes at 0.60 is buying 100 no at 0.40
    exchange
        .create_order(&limit_order(
            "KXNO",
            OrderSide::Ask,
            100,
            "0.60",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    // fee 0.07 * 0.60 * 0.40 * 100 = 1.68
    assert_eq!(exchange.balance(), d("1058.32"));
    assert_eq!(
        exchange.positions().pop().expect("position").position_fp,
        d("-100")
    );

    let settlement = exchange.settle("KXNO", d("0.00")).unwrap();
    assert_eq!(settlement.market_result, MarketResult::No);
    assert_eq!(settlement.no_count_fp, d("100"));
    assert_eq!(settlement.revenue, 0);
    assert_eq!(settlement.value, Some(0));
    // the no position paid out: the credited face value stays, the cost was 40
    assert_eq!(exchange.balance(), d("1058.32"));

    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXSC");
    exchange
        .add_liquidity("KXSC", OrderSide::Ask, price("0.50"), Count::from(100))
        .unwrap();
    exchange
        .create_order(&limit_order(
            "KXSC",
            OrderSide::Bid,
            100,
            "0.50",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    // fee 0.07 * 0.50 * 0.50 * 100 = 1.75
    assert_eq!(exchange.balance(), d("948.25"));
    let settlement = exchange.settle("KXSC", d("0.30")).unwrap();
    assert_eq!(settlement.market_result, MarketResult::Scalar);
    assert_eq!(settlement.revenue, 3_000);
    assert_eq!(settlement.value, Some(30));
    assert_eq!(exchange.balance(), d("978.25"));

    // an out-of-range value is rejected on a market that has not settled
    exchange.add_market("KXOO");
    match exchange.settle("KXOO", d("1.50")) {
        Err(ExchangeError::SettlementValueOutOfRange { value }) => {
            assert_eq!(value, d("1.50"));
        }
        other => panic!("expected SettlementValueOutOfRange, got {other:?}"),
    }
}

#[test]
fn fees_follow_the_schedule_and_charge_makers() {
    let exchange = FakeExchange::new(d("1000"))
        .fee_schedule(furball::types::FeeType::QuadraticWithMakerFees, d("2.0"));
    exchange.add_market("KXTEST");
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.50",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();

    // the taker crosses its own resting ask at 0.50
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.55",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    let fills = exchange.fills(None);
    assert_eq!(fills.len(), 2);
    let taker = fills.iter().find(|fill| fill.is_taker).expect("taker");
    // 2.0 * 0.07 * 0.50 * 0.50 * 100 = 3.50
    assert_eq!(taker.fee_cost, d("3.50"));
    let maker = fills.iter().find(|fill| !fill.is_taker).expect("maker");
    // 2.0 * 0.0175 * 0.50 * 0.50 * 100 = 0.875, revenue +50 -> net 0.88
    assert_eq!(maker.fee_cost, d("0.88"));
    // the self-trade nets the position; only the 4.38 of fees leaves the account
    assert_eq!(exchange.balance(), d("995.62"));
    assert_eq!(exchange.positions()[0].position_fp, d("0"));
}

#[test]
fn expired_orders_cancel_when_the_clock_moves() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    let order = OrderBuilder::new(
        "KXTEST",
        OrderSide::Bid,
        Count::from(100),
        "0.50".parse().expect("price"),
        TimeInForce::GoodTillCanceled,
        SelfTradePrevention::TakerAtCross,
    )
    .client_order_id("expires")
    .expiration_time(ts("2026-01-01T01:00:00Z").timestamp_millis())
    .build();
    let order = exchange.create_order(&order).unwrap();
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Resting
    );

    exchange.set_time(ts("2026-01-01T01:30:00Z"));
    assert_eq!(
        exchange.order(&order.order_id).unwrap().status,
        OrderStatus::Canceled
    );
    assert!(
        exchange
            .order_book("KXTEST")
            .unwrap()
            .yes_dollars
            .is_empty()
    );
    assert_eq!(exchange.balance(), d("1000"));
}

#[test]
fn an_unaffordable_buy_is_rejected_and_the_sell_is_not() {
    let exchange = FakeExchange::new(d("10"));
    exchange.add_market("KXTEST");
    match exchange.create_order(&limit_order(
        "KXTEST",
        OrderSide::Bid,
        100,
        "0.50",
        TimeInForce::GoodTillCanceled,
    )) {
        Err(ExchangeError::InsufficientBalance {
            required,
            available,
            ..
        }) => {
            assert_eq!(required, d("50"));
            assert_eq!(available, d("10"));
        }
        other => panic!("expected InsufficientBalance, got {other:?}"),
    }
    // a sell needs no reserve
    let order = exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.50",
            TimeInForce::GoodTillCanceled,
        ))
        .unwrap();
    assert_eq!(order.remaining_count, d("100"));
    assert_eq!(exchange.balance(), d("10"));
}

#[test]
fn a_two_leg_strategy_captures_the_spread() {
    let exchange = FakeExchange::new(d("1000"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Bid,
            100,
            "0.55",
            TimeInForce::ImmediateOrCancel,
        ))
        .unwrap();
    assert_eq!(
        exchange.positions().pop().expect("position").position_fp,
        d("100")
    );

    // the market moves up; the strategy sells into the new bid
    exchange.set_last_price("KXTEST", d("0.65")).unwrap();
    assert_eq!(exchange.last_price("KXTEST"), Some(d("0.65")));
    exchange
        .add_liquidity("KXTEST", OrderSide::Bid, price("0.65"), Count::from(100))
        .unwrap();
    exchange
        .create_order(&limit_order(
            "KXTEST",
            OrderSide::Ask,
            100,
            "0.65",
            TimeInForce::ImmediateOrCancel,
        ))
        .unwrap();

    let position = exchange.positions().pop().expect("position");
    assert_eq!(position.position_fp, d("0"));
    assert_eq!(position.realized_pnl_dollars, d("10.00"));
    assert_eq!(position.fees_paid_dollars, d("3.34"));
    assert_eq!(exchange.balance(), d("1006.66"));
}

#[test]
fn undocumented_fee_type_rejects_order_without_touching_state() {
    let exchange = FakeExchange::new(d("1000")).fee_schedule(furball::types::FeeType::Flat, d("1"));
    exchange.add_market("KXTEST");
    exchange
        .add_liquidity("KXTEST", OrderSide::Ask, price("0.55"), Count::from(100))
        .unwrap();

    let result = exchange.create_order(&limit_order(
        "KXTEST",
        OrderSide::Bid,
        10,
        "0.60",
        TimeInForce::GoodTillCanceled,
    ));

    assert!(matches!(result, Err(ExchangeError::Fee(_))));
    assert_eq!(exchange.balance(), d("1000"));
}
