use chrono::{DateTime, Utc};
use furball::types::{BookSide, Fill, Market, OutcomeSide};
use furball::{
    Count, PnlError, Price, SettlementError, average_entry_price, payoff_at_settlement,
    realized_pnl, unrealized_pnl,
};
use rust_decimal::Decimal;
use std::str::FromStr;

fn market(result: &str, settlement: Option<&str>) -> Market {
    let now: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().expect("timestamp");
    Market {
        ticker: "KXTEST".to_owned(),
        event_ticker: "KXTEST-E".to_owned(),
        market_type: "binary".to_owned(),
        status: "finalized".to_owned(),
        title: None,
        open_time: now,
        close_time: now,
        latest_expiration_time: now,
        settlement_ts: None,
        result: result.to_owned(),
        settlement_value_dollars: settlement.map(|value| value.parse().expect("decimal")),
        yes_bid_dollars: Decimal::new(0, 0),
        yes_ask_dollars: Decimal::new(0, 0),
        last_price_dollars: Decimal::new(0, 0),
        volume_fp: Decimal::new(0, 0),
        volume_24h_fp: Decimal::new(0, 0),
        open_interest_fp: Decimal::new(0, 0),
        price_level_structure: None,
        price_ranges: None,
    }
}

fn scalar_market(settlement: Option<&str>) -> Market {
    let mut market = market("scalar", settlement);
    market.market_type = "scalar".to_owned();
    market
}

fn fill(outcome: OutcomeSide, yes_price: &str, no_price: &str, count: &str, fee: &str) -> Fill {
    Fill {
        fill_id: "f".to_owned(),
        exchange_index: 0,
        trade_id: "f".to_owned(),
        order_id: "o".to_owned(),
        ticker: "KXTEST".to_owned(),
        market_ticker: "KXTEST".to_owned(),
        outcome_side: outcome,
        book_side: match outcome {
            OutcomeSide::Yes => BookSide::Bid,
            OutcomeSide::No => BookSide::Ask,
        },
        count_fp: count.parse().expect("count"),
        yes_price_dollars: yes_price.parse().expect("price"),
        no_price_dollars: no_price.parse().expect("price"),
        is_taker: true,
        fee_cost: fee.parse().expect("fee"),
        created_time: None,
        subaccount_number: None,
    }
}

fn hundred(count: u32) -> Count {
    Count::from(count)
}

#[test]
fn binary_payoff_follows_the_result() {
    let yes = market("yes", Some("1.00"));
    let no = market("no", Some("0.00"));

    assert_eq!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(100), &yes).expect("yes on yes"),
        "100".parse().expect("decimal")
    );
    assert_eq!(
        payoff_at_settlement(OutcomeSide::No, hundred(100), &yes).expect("no on yes"),
        Decimal::new(0, 0)
    );
    assert_eq!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(100), &no).expect("yes on no"),
        Decimal::new(0, 0)
    );
    assert_eq!(
        payoff_at_settlement(OutcomeSide::No, hundred(100), &no).expect("no on no"),
        "100".parse().expect("decimal")
    );
}

#[test]
fn scalar_payoff_uses_the_settlement_value() {
    let settled = scalar_market(Some("0.30"));

    // yes pays 0.30 per contract, no pays the complement
    assert_eq!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(100), &settled).expect("yes"),
        "30".parse().expect("decimal")
    );
    assert_eq!(
        payoff_at_settlement(OutcomeSide::No, hundred(100), &settled).expect("no"),
        "70".parse().expect("decimal")
    );
}

#[test]
fn unsettled_and_bad_settlements_error() {
    let pending = market("", None);
    assert!(matches!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(1), &pending),
        Err(SettlementError::NotSettled { result, .. }) if result.is_empty()
    ));

    let unknown = market("mystery", None);
    assert!(matches!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(1), &unknown),
        Err(SettlementError::NotSettled { ref result, .. }) if result == "mystery"
    ));

    let valueless = scalar_market(None);
    assert!(matches!(
        payoff_at_settlement(OutcomeSide::Yes, hundred(1), &valueless),
        Err(SettlementError::MissingSettlementValue { .. })
    ));

    let runaway = scalar_market(Some("1.50"));
    assert!(matches!(
        payoff_at_settlement(OutcomeSide::No, hundred(1), &runaway),
        Err(SettlementError::ValueOutOfRange { value, .. }) if value == "1.50".parse().expect("decimal")
    ));
}

#[test]
fn realized_pnl_closes_in_several_fills() {
    // Buy 100 yes at 0.50, then sell the position in two fills: 60 at yes 0.60
    // and 40 at yes 0.65. A yes sale arrives as a no-side fill priced at the
    // no price.
    let fills = vec![
        fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.05"),
        fill(OutcomeSide::No, "0.60", "0.40", "60", "0.03"),
        fill(OutcomeSide::No, "0.65", "0.35", "40", "0.04"),
    ];
    // proceeds 60 * 0.60 + 40 * 0.65 = 36 + 26 = 62; cost 50; fees 0.12
    assert_eq!(
        realized_pnl(&fills).expect("closed"),
        "11.88".parse().expect("decimal")
    );
}

#[test]
fn realized_pnl_on_a_no_position() {
    // Buy 100 no at no 0.40 (yes 0.60), sell 100 no at no 0.30 (yes 0.70).
    let fills = vec![
        fill(OutcomeSide::No, "0.60", "0.40", "100", "0.04"),
        fill(OutcomeSide::Yes, "0.70", "0.30", "100", "0.03"),
    ];
    // 100 * (0.30 - 0.40) = -10, minus 0.07 of fees
    assert_eq!(
        realized_pnl(&fills).expect("closed"),
        "-10.07".parse().expect("decimal")
    );
}

#[test]
fn realized_pnl_of_a_paired_market_captures_the_face_value() {
    // Buying a yes and a no is buying the whole market: together they always
    // settle at $1, so the pair bought for 0.50 + 0.40 locks in 0.10 per
    // contract, whatever the result.
    let fills = vec![
        fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.03"),
        fill(OutcomeSide::No, "0.60", "0.40", "100", "0.02"),
    ];
    assert_eq!(
        realized_pnl(&fills).expect("closed"),
        "9.95".parse().expect("decimal")
    );
}

#[test]
fn realized_pnl_requires_a_closed_position() {
    let fills = vec![fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.05")];
    match realized_pnl(&fills) {
        Err(PnlError::OpenPosition { yes, no }) => {
            assert_eq!(yes, "100".parse().expect("decimal"));
            assert_eq!(no, Decimal::new(0, 0));
        }
        other => panic!("expected OpenPosition, got {other:?}"),
    }
}

#[test]
fn unrealized_pnl_marks_the_open_position() {
    let yes_fill = fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.05");
    // -50.05 of cost against 100 marked at 0.60
    assert_eq!(
        unrealized_pnl(&[yes_fill], Price::from_str("0.60").expect("mark")).expect("yes"),
        "9.95".parse().expect("decimal")
    );

    let no_fill = fill(OutcomeSide::No, "0.60", "0.40", "100", "0.05");
    // 100 no marked at 1 - 0.55 = 0.45: worth 45, cost 40, fee 0.05
    assert_eq!(
        unrealized_pnl(&[no_fill], Price::from_str("0.55").expect("mark")).expect("no"),
        "4.95".parse().expect("decimal")
    );
}

#[test]
fn unrealized_pnl_of_a_flat_position_is_just_the_negative_fees() {
    let fills = vec![
        fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.05"),
        fill(OutcomeSide::No, "0.50", "0.50", "100", "0.05"),
    ];
    assert_eq!(
        unrealized_pnl(&fills, Price::from_str("0.50").expect("mark")).expect("flat"),
        "-0.10".parse().expect("decimal")
    );
}

#[test]
fn average_entry_price_is_volume_weighted() {
    let yes_fills = vec![
        fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.01"),
        fill(OutcomeSide::Yes, "0.60", "0.40", "100", "0.01"),
    ];
    assert_eq!(
        average_entry_price(&yes_fills, OutcomeSide::Yes).expect("yes"),
        "0.55".parse().expect("decimal")
    );

    // 100 at 0.50 and 50 at 0.60: (50 + 30) / 150
    let uneven = vec![
        fill(OutcomeSide::Yes, "0.50", "0.50", "100", "0.01"),
        fill(OutcomeSide::Yes, "0.60", "0.40", "50", "0.01"),
    ];
    assert_eq!(
        average_entry_price(&uneven, OutcomeSide::Yes).expect("uneven"),
        "80".parse::<Decimal>().expect("decimal") / "150".parse::<Decimal>().expect("decimal")
    );

    // no entries price at the no price: (40 + 30) / 200
    let no_fills = vec![
        fill(OutcomeSide::No, "0.60", "0.40", "100", "0.01"),
        fill(OutcomeSide::No, "0.70", "0.30", "100", "0.01"),
    ];
    assert_eq!(
        average_entry_price(&no_fills, OutcomeSide::No).expect("no"),
        "0.35".parse().expect("decimal")
    );
}

#[test]
fn average_entry_price_errors_without_fills_on_the_side() {
    let no_fill = fill(OutcomeSide::No, "0.60", "0.40", "100", "0.01");
    assert!(matches!(
        average_entry_price(&[no_fill], OutcomeSide::Yes),
        Err(PnlError::NoFills(OutcomeSide::Yes))
    ));
}
