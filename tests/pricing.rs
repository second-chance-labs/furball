use furball::types::{Market, PriceRange};
use furball::{Count, CountError, Price, PriceError};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rust_decimal::Decimal;
use serde_json::json;

fn market(ticker: &str, structure: Option<&str>, ranges: Option<Vec<PriceRange>>) -> Market {
    let ts: chrono::DateTime<chrono::Utc> = "2026-01-01T00:00:00Z".parse().expect("ts");
    Market {
        ticker: ticker.to_owned(),
        event_ticker: "KXEVENT".to_owned(),
        market_type: "binary".to_owned(),
        status: "active".to_owned(),
        title: None,
        open_time: ts,
        close_time: ts,
        latest_expiration_time: ts,
        settlement_ts: None,
        result: String::new(),
        settlement_value_dollars: None,
        yes_bid_dollars: "0.50".parse().expect("decimal"),
        yes_ask_dollars: "0.50".parse().expect("decimal"),
        last_price_dollars: "0.50".parse().expect("decimal"),
        volume_fp: "0.00".parse().expect("decimal"),
        volume_24h_fp: "0.00".parse().expect("decimal"),
        open_interest_fp: "0.00".parse().expect("decimal"),
        price_level_structure: structure.map(str::to_owned),
        price_ranges: ranges,
    }
}

fn range(start: &str, end: &str, step: &str) -> PriceRange {
    PriceRange {
        start: start.parse().expect("decimal"),
        end: end.parse().expect("decimal"),
        step: step.parse().expect("decimal"),
    }
}

#[test]
fn price_accepts_only_the_open_unit_interval() {
    assert!(Price::new(Decimal::new(0, 0)).is_err());
    assert!(Price::new(Decimal::new(1, 0)).is_err());
    for good in ["0.0001", "0.55", "0.9999"] {
        assert!(Price::new(good.parse().expect("decimal")).is_ok(), "{good}");
    }
    for bad in ["0", "1", "-0.1", "1.01", "100"] {
        let value: Decimal = bad.parse().expect("decimal");
        assert!(
            matches!(Price::new(value), Err(PriceError::OutOfRange(actual)) if actual == value),
            "{bad}"
        );
        assert!(matches!(
            bad.parse::<Price>(),
            Err(PriceError::OutOfRange(_))
        ));
    }
    assert!(matches!(
        "not-a-number".parse::<Price>(),
        Err(PriceError::Parse(_))
    ));
}

#[test]
fn property_valid_prices_round_trip() {
    let mut rng = StdRng::seed_from_u64(18);
    for _ in 0..1000 {
        let scale = rng.gen_range(2..=4);
        let max_units = 10u32.pow(scale) - 1;
        let units = rng.gen_range(1..=max_units);
        let dollars = Decimal::new(units as i64, scale);
        let price = Price::new(dollars).expect("on-grid value in (0, 1)");
        assert_eq!(price.as_dollars(), dollars);
        let reparsed: Price = price.to_string().parse().expect("string round trip");
        assert_eq!(reparsed, price);
        assert_eq!(Decimal::from(reparsed), dollars);
    }
}

#[test]
fn property_invalid_prices_error() {
    let mut rng = StdRng::seed_from_u64(19);
    for _ in 0..1000 {
        let scale = rng.gen_range(1..=4);
        let value = Decimal::new(rng.gen_range(0..=10u32.pow(scale)) as i64, scale);
        let edge = rng.gen_bool(0.5);
        let candidate = if edge {
            if value.is_zero() {
                Decimal::new(1, 0)
            } else {
                Decimal::new(0, 0)
            }
        } else if rng.gen_bool(0.5) {
            Decimal::new(1, 0) + value
        } else {
            -value - Decimal::new(1, 0)
        };
        assert!(
            matches!(Price::new(candidate), Err(PriceError::OutOfRange(actual)) if actual == candidate),
            "{candidate}"
        );
    }
}

#[test]
fn property_off_tick_prices_fail_validation() {
    let cent = market("KXCENT", Some("linear_cent"), None);
    let mill = market("KXMILL", Some("linear_mill"), None);
    let millicent = market("KXMILLCENT", Some("linear_millicent"), None);
    let mut rng = StdRng::seed_from_u64(20);
    for _ in 0..1000 {
        let units = rng.gen_range(1..=999);
        let dollars = Decimal::new(units as i64 * 10 + 5, 4);
        let price = Price::new(dollars).expect("millicent-grid value in (0, 1)");
        match price.validate(&cent) {
            Err(PriceError::OffTick { tick, .. }) => assert_eq!(tick, "0.01".parse().expect("d")),
            other => panic!("expected OffTick on cent market, got {other:?}"),
        }
        assert!(matches!(
            price.validate(&mill),
            Err(PriceError::OffTick { .. })
        ));
        price.validate(&millicent).expect("on the millicent grid");
    }
}

#[test]
fn property_cents_round_trip() {
    let mut rng = StdRng::seed_from_u64(21);
    for _ in 0..1000 {
        let units = rng.gen_range(1..=9999);
        let cents = Decimal::new(units as i64, 2);
        let price = Price::from_cents(cents).expect("cents in (0, 100)");
        assert_eq!(price.to_cents(), cents);
        assert_eq!(
            price,
            Price::new(cents / Decimal::new(100, 0)).expect("dollars")
        );
    }
}

#[test]
fn cents_conversions_are_hand_computed() {
    let price = Price::from_cents(Decimal::new(55, 0)).expect("55 cents");
    assert_eq!(price, "0.55".parse::<Price>().expect("dollars"));
    assert_eq!(price.to_cents(), Decimal::new(55, 0));
    assert_eq!(
        Price::from_cents("7.1".parse().expect("decimal")).expect("sub-cent"),
        "0.071".parse::<Price>().expect("dollars")
    );
    assert!(Price::from_cents(Decimal::new(100, 0)).is_err());
    assert!(Price::from_cents(Decimal::new(0, 0)).is_err());
}

#[test]
fn tick_size_comes_from_price_ranges_when_structure_is_not_linear() {
    let ranges = Some(vec![
        range("0.0000", "0.1000", "0.0100"),
        range("0.1000", "1.0000", "0.00100"),
    ]);
    let market = market("KXRANGE", Some("custom"), ranges.clone());

    assert_eq!(
        "0.0500"
            .parse::<Price>()
            .expect("price")
            .tick_size(&market)
            .expect("tick"),
        "0.01".parse().expect("d")
    );
    assert_eq!(
        "0.5550"
            .parse::<Price>()
            .expect("price")
            .tick_size(&market)
            .expect("tick"),
        "0.001".parse().expect("d")
    );
    assert!(matches!(
        "0.5555".parse::<Price>().expect("price").validate(&market),
        Err(PriceError::OffTick { .. })
    ));
    "0.5550"
        .parse::<Price>()
        .expect("price")
        .validate(&market)
        .expect("on grid");
    let boundary = "0.1000".parse::<Price>().expect("price");
    boundary
        .validate(&market)
        .expect("boundary price is covered");
}

#[test]
fn tick_lookup_reports_unknown_structures_and_bad_ranges() {
    let missing = market("KXMISSING", None, None);
    assert!(matches!(
        "0.55".parse::<Price>().expect("price").tick_size(&missing),
        Err(PriceError::UnknownStructure { structure, .. }) if structure.is_empty()
    ));
    let unknown = market("KXUNKNOWN", Some("exotic"), None);
    assert!(matches!(
        "0.55".parse::<Price>().expect("price").tick_size(&unknown),
        Err(PriceError::UnknownStructure { ref structure, .. }) if structure == "exotic"
    ));
    let uncovered = market(
        "KXGAP",
        Some("exotic"),
        Some(vec![range("0.0000", "0.1000", "0.0100")]),
    );
    assert!(matches!(
        "0.55"
            .parse::<Price>()
            .expect("price")
            .tick_size(&uncovered),
        Err(PriceError::NoPriceRange { .. })
    ));
    let bad_tick = market(
        "KXBADTICK",
        Some("exotic"),
        Some(vec![range("0.0000", "1.0000", "0.0000")]),
    );
    assert!(matches!(
        "0.55".parse::<Price>().expect("price").tick_size(&bad_tick),
        Err(PriceError::InvalidTick { .. })
    ));
}

#[test]
fn count_accepts_non_negative_values_and_rejects_negatives() {
    for good in ["0", "0.00", "1", "25.5"] {
        assert!(Count::new(good.parse().expect("decimal")).is_ok(), "{good}");
    }
    for bad in ["-0.01", "-1", "-25.5"] {
        let value: Decimal = bad.parse().expect("decimal");
        assert!(
            matches!(Count::new(value), Err(CountError::Negative(actual)) if actual == value),
            "{bad}"
        );
    }
    assert!(matches!(
        "not-a-number".parse::<Count>(),
        Err(CountError::Parse(_))
    ));
}

#[test]
fn property_counts_round_trip_and_negatives_error() {
    let mut rng = StdRng::seed_from_u64(22);
    for _ in 0..1000 {
        let value = Decimal::new(rng.gen_range(0..=100_000) as i64, rng.gen_range(0..=2));
        let count = Count::new(value).expect("non-negative");
        assert_eq!(count.as_contracts(), value);
        let reparsed: Count = count.to_string().parse().expect("string round trip");
        assert_eq!(reparsed, count);
        let negative = -value - Decimal::new(rng.gen_range(1..=10) as i64, 0);
        assert!(
            matches!(Count::new(negative), Err(CountError::Negative(actual)) if actual == negative)
        );
    }
}

#[test]
fn price_and_count_serialize_as_fixed_point_strings() {
    let price: Price = "0.5500".parse().expect("price");
    assert_eq!(serde_json::to_value(price).expect("json"), json!("0.5500"));
    let count: Count = "10.00".parse().expect("count");
    assert_eq!(serde_json::to_value(count).expect("json"), json!("10.00"));
    let round_tripped: Price =
        serde_json::from_value(json!("0.5500")).expect("price json round trip");
    assert_eq!(round_tripped, price);
}

#[test]
fn count_builds_from_plain_integers() {
    let count: Count = 10.into();
    assert_eq!(count, Count::new(Decimal::new(10, 0)).expect("count"));
    let count: Count = 1u64.into();
    assert_eq!(count.as_contracts(), Decimal::new(1, 0));
}
