use std::sync::Mutex;

use chrono::{DateTime, Utc};
use furball::types::FeeType;
use furball::{
    BalancePrecision, Count, FEE_COMBO_MAKER_ENV, FEE_MAKER_ENV, FEE_TAKER_ENV, FeeAccumulator,
    FeeCalculator, FeeCoefficientError, FeeCoefficients, FeeError, FeeScheduleRow, FeeSide, Price,
    fee_in_force,
};
use rust_decimal::Decimal;
use std::str::FromStr;

fn ts(text: &str) -> DateTime<Utc> {
    text.parse().expect("timestamp")
}

fn row(
    series: &str,
    event: Option<&str>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    fee_type: Option<&str>,
    multiplier: Option<&str>,
) -> FeeScheduleRow {
    FeeScheduleRow {
        series_ticker: series.to_owned(),
        event_ticker: event.map(str::to_owned),
        effective_from: from,
        effective_to: to,
        fee_type: fee_type.map(str::to_owned),
        fee_multiplier: multiplier.map(|text| text.parse().expect("decimal")),
        source: "test".to_owned(),
    }
}

#[test]
fn default_coefficients_are_pinned() {
    let coefficients = FeeCoefficients::default();
    assert_eq!(coefficients.taker, "0.07".parse().expect("decimal"));
    assert_eq!(coefficients.maker, "0.0175".parse().expect("decimal"));
    assert_eq!(coefficients.combo_maker, "0.035".parse().expect("decimal"));
    assert_eq!(
        coefficients.source_date,
        chrono::NaiveDate::from_ymd_opt(2026, 11, 9).expect("date")
    );
    assert_eq!(coefficients.source_url, "https://kalshi.com/fee-schedule");
}

static ENV: Mutex<()> = Mutex::new(());

fn reset_env() {
    // SAFETY: the ENV mutex makes this the only test touching these variables.
    unsafe {
        for variable in [FEE_TAKER_ENV, FEE_MAKER_ENV, FEE_COMBO_MAKER_ENV] {
            std::env::remove_var(variable);
        }
    }
}

fn set_env(variable: &str, value: &str) {
    // SAFETY: the ENV mutex makes this the only test touching these variables.
    unsafe { std::env::set_var(variable, value) };
}

#[test]
fn from_env_uses_defaults_when_unset() {
    let _guard = ENV.lock().expect("env lock");
    reset_env();
    assert_eq!(
        FeeCoefficients::from_env().expect("defaults"),
        FeeCoefficients::default()
    );
}

#[test]
fn from_env_overrides_each_variable_and_rejects_invalid() {
    let _guard = ENV.lock().expect("env lock");
    reset_env();

    set_env(FEE_MAKER_ENV, "not-a-number");
    match FeeCoefficients::from_env() {
        Err(FeeCoefficientError::NotADecimal { variable, value }) => {
            assert_eq!(variable, FEE_MAKER_ENV);
            assert_eq!(value, "not-a-number");
        }
        other => panic!("expected NotADecimal, got {other:?}"),
    }

    set_env(FEE_TAKER_ENV, "0.08");
    set_env(FEE_MAKER_ENV, "0.02");
    set_env(FEE_COMBO_MAKER_ENV, "0.04");
    let coefficients = FeeCoefficients::from_env().expect("overridden");
    assert_eq!(coefficients.taker, "0.08".parse().expect("decimal"));
    assert_eq!(coefficients.maker, "0.02".parse().expect("decimal"));
    assert_eq!(coefficients.combo_maker, "0.04".parse().expect("decimal"));
    assert_eq!(
        coefficients.source_date,
        FeeCoefficients::default().source_date
    );
    assert_eq!(
        coefficients.source_url,
        FeeCoefficients::default().source_url
    );
    reset_env();
}

#[test]
fn fcm_fill_example_from_the_rounding_docs() {
    // https://docs.kalshi.com/getting_started/fee_rounding: a non-direct member's
    // FCM-cleared fill with -$0.055000 revenue and a $0.00363825 model fee.
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let fee = calculator.round_fill(
        "0.00363825".parse().expect("model fee"),
        "-0.055".parse().expect("revenue"),
        BalancePrecision::Cent,
        &mut accumulator,
    );
    assert_eq!(fee.trade_fee, "0.003639".parse().expect("decimal"));
    assert_eq!(fee.rounding_fee, "0.001361".parse().expect("decimal"));
    assert_eq!(fee.rebate, Decimal::new(0, 0));
    assert_eq!(fee.net_fee, "0.005".parse().expect("decimal"));
}

#[test]
fn accumulator_rebate_table_from_the_rounding_docs() {
    // Three fills adding $0.004 each, non-direct precision:
    // 1 | 0.004 | 0.004 | -    | 0.004
    // 2 | 0.004 | 0.008 | -    | 0.008
    // 3 | 0.004 | 0.012 | 0.010| 0.002
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let added: Decimal = "0.004".parse().expect("decimal");
    for index in 1..=3 {
        let before_rebate = accumulator.carried() + added;
        let rebate = accumulator.add_rounding(added);
        assert_eq!(before_rebate, Decimal::new(index, 0) * added);
        if index < 3 {
            assert_eq!(rebate, Decimal::new(0, 0));
            assert_eq!(accumulator.carried(), before_rebate);
        } else {
            assert_eq!(rebate, "0.010".parse().expect("decimal"));
            assert_eq!(accumulator.carried(), "0.002".parse().expect("decimal"));
        }
    }
}

#[test]
fn ten_thousandth_precision_aligns_like_the_docs() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::TenThousandth);
    let fee = calculator.round_fill(
        "0.00363825".parse().expect("model fee"),
        "-0.055".parse().expect("revenue"),
        BalancePrecision::TenThousandth,
        &mut accumulator,
    );
    // exact = -0.058639, aligned to -0.0587 (toward -inf), rounding fee 0.000061.
    assert_eq!(fee.trade_fee, "0.003639".parse().expect("decimal"));
    assert_eq!(fee.rounding_fee, "0.000061".parse().expect("decimal"));
    assert_eq!(fee.net_fee, "0.0037".parse().expect("decimal"));
}

#[test]
fn the_net_fee_never_goes_negative() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    // Fill one builds up 0.006 of carried overpayment: fee 0.01.
    let first = calculator.round_fill(
        "0.004".parse().expect("model fee"),
        "-0.05".parse().expect("revenue"),
        BalancePrecision::Cent,
        &mut accumulator,
    );
    assert_eq!(first.net_fee, "0.01".parse().expect("decimal"));
    assert_eq!(first.rebate, Decimal::new(0, 0));
    // Fill two: 0.000001 + 0.005999 = 0.006 of fee against a 0.01 rebate.
    let second = calculator.round_fill(
        "0.000001".parse().expect("model fee"),
        "-0.014".parse().expect("revenue"),
        BalancePrecision::Cent,
        &mut accumulator,
    );
    assert_eq!(second.trade_fee, "0.000001".parse().expect("decimal"));
    assert_eq!(second.rounding_fee, "0.005999".parse().expect("decimal"));
    assert_eq!(second.rebate, "0.010".parse().expect("decimal"));
    assert_eq!(second.net_fee, Decimal::new(0, 0));
}

#[test]
fn quadratic_formulas_are_hand_computed() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let price: Price = "0.50".parse().expect("price");
    let count: Count = "10".parse().expect("count");
    let one = Decimal::new(1, 0);

    // 0.07 * 0.5 * 0.5 * 10 = 0.175
    assert_eq!(
        calculator
            .model_fee(FeeType::Quadratic, one, FeeSide::Taker, price, count)
            .expect("taker"),
        "0.175".parse().expect("decimal")
    );
    // quadratic has no maker fee
    assert_eq!(
        calculator
            .model_fee(FeeType::Quadratic, one, FeeSide::Maker, price, count)
            .expect("maker"),
        Decimal::new(0, 0)
    );
    // 0.0175 * 0.25 * 10 = 0.04375
    assert_eq!(
        calculator
            .model_fee(
                FeeType::QuadraticWithMakerFees,
                one,
                FeeSide::Maker,
                price,
                count
            )
            .expect("maker"),
        "0.04375".parse().expect("decimal")
    );
    // 0.035 * 0.25 * 10 = 0.0875
    assert_eq!(
        calculator
            .model_fee(
                FeeType::QuadraticWithComboMakerFees,
                one,
                FeeSide::Maker,
                price,
                count
            )
            .expect("combo maker"),
        "0.0875".parse().expect("decimal")
    );
    // multiplier 2 doubles the taker fee
    assert_eq!(
        calculator
            .model_fee(
                FeeType::Quadratic,
                "2.0".parse().expect("d"),
                FeeSide::Taker,
                price,
                count
            )
            .expect("scaled"),
        "0.35".parse().expect("decimal")
    );
}

#[test]
fn undocumented_fee_types_error() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let price: Price = "0.50".parse().expect("price");
    let count: Count = "10".parse().expect("count");
    assert!(matches!(
        calculator.model_fee(FeeType::Flat, Decimal::new(1, 0), FeeSide::Taker, price, count),
        Err(FeeError::UndocumentedFormula(name)) if name == "flat"
    ));
    assert!(matches!(
        calculator.model_fee(
            FeeType::Other("mystery".to_owned()),
            Decimal::new(1, 0),
            FeeSide::Taker,
            price,
            count
        ),
        Err(FeeError::UndocumentedFormula(name)) if name == "mystery"
    ));
}

#[test]
fn trade_fee_rounds_up_to_a_microdollar() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let fee = calculator.round_fill(
        "0.0000011".parse().expect("model fee"),
        Decimal::new(0, 0),
        BalancePrecision::Cent,
        &mut accumulator,
    );
    assert_eq!(fee.trade_fee, "0.000002".parse().expect("decimal"));
    // revenue minus the trade fee lands exactly on the cent grid: no rounding fee
    let fee = calculator.round_fill(
        "0.000001".parse().expect("model fee"),
        "-0.009999".parse().expect("revenue"),
        BalancePrecision::Cent,
        &mut accumulator,
    );
    assert_eq!(fee.trade_fee, "0.000001".parse().expect("decimal"));
    assert_eq!(fee.rounding_fee, Decimal::new(0, 0));
    assert_eq!(fee.net_fee, "0.000001".parse().expect("decimal"));
}

#[test]
fn maker_fill_on_quadratic_owes_nothing() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let price: Price = "0.40".parse().expect("price");
    let count: Count = "100".parse().expect("count");
    let fee = calculator
        .fill_fee(
            FeeType::Quadratic,
            Decimal::new(1, 0),
            FeeSide::Maker,
            price,
            count,
            "40.00".parse().expect("revenue"),
            BalancePrecision::Cent,
            &mut accumulator,
        )
        .expect("maker fill");
    assert_eq!(fee.model_fee, Decimal::new(0, 0));
    assert_eq!(fee.net_fee, Decimal::new(0, 0));
}

#[test]
fn fee_in_force_resolves_series_and_event_overrides() {
    let before = ts("2026-01-01T00:00:00Z");
    let middle = ts("2026-02-01T00:00:00Z");
    let after = ts("2026-03-01T00:00:00Z");
    let rows = vec![
        row(
            "KX",
            None,
            Some(before),
            Some(middle),
            Some("quadratic"),
            Some("1.0"),
        ),
        row(
            "KX",
            None,
            Some(middle),
            None,
            Some("quadratic_with_maker_fees"),
            Some("2.0"),
        ),
        row(
            "KX",
            Some("KX-E1"),
            Some(middle),
            None,
            Some("quadratic_with_combo_maker_fees"),
            Some("3.0"),
        ),
    ];

    let fee =
        fee_in_force(&rows, "KX", None, before).expect("first interval is inclusive at its start");
    assert_eq!(fee.fee_type, FeeType::Quadratic);
    assert_eq!(fee.multiplier, "1.0".parse().expect("decimal"));

    // the first interval is exclusive at its end
    assert!(matches!(
        fee_in_force(&rows, "KX", None, middle),
        Ok(ref scheduled) if scheduled.multiplier == "2.0".parse().expect("decimal")
    ));

    let fee = fee_in_force(&rows, "KX", Some("KX-E1"), after).expect("override");
    assert_eq!(fee.fee_type, FeeType::QuadraticWithComboMakerFees);
    assert_eq!(fee.multiplier, "3.0".parse().expect("decimal"));

    // no override for this event: the series row applies
    let fee = fee_in_force(&rows, "KX", Some("KX-E2"), after).expect("series row");
    assert_eq!(fee.fee_type, FeeType::QuadraticWithMakerFees);
    assert_eq!(fee.multiplier, "2.0".parse().expect("decimal"));

    assert!(matches!(
        fee_in_force(&rows, "KX-MISSING", None, after),
        Err(FeeError::NoFeeRow { .. })
    ));
    assert!(matches!(
        fee_in_force(&rows, "KX", None, ts("2025-12-31T23:59:59Z")),
        Err(FeeError::NoFeeRow { .. })
    ));
}

#[test]
fn fee_in_force_inherits_series_fields_from_sparse_overrides() {
    let at = ts("2026-02-01T00:00:00Z");
    let rows = vec![
        row("KX", None, None, None, Some("quadratic"), Some("1.0")),
        row("KX", Some("KX-E1"), Some(at), None, None, Some("5.0")),
    ];
    let fee = fee_in_force(&rows, "KX", Some("KX-E1"), at).expect("sparse override");
    assert_eq!(fee.fee_type, FeeType::Quadratic);
    assert_eq!(fee.multiplier, "5.0".parse().expect("decimal"));
}

#[test]
fn an_end_to_end_fill_matches_the_hand_computation() {
    // Series: quadratic_with_maker_fees, multiplier 1. Taker buys 1 yes at 0.55;
    // the trade changes the balance by -0.55.
    let rows = vec![row(
        "KX",
        None,
        None,
        None,
        Some("quadratic_with_maker_fees"),
        Some("1.0"),
    )];
    let scheduled = fee_in_force(&rows, "KX", None, ts("2026-02-01T00:00:00Z")).expect("fee");
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let fee = calculator
        .fill_fee(
            scheduled.fee_type,
            scheduled.multiplier,
            FeeSide::Taker,
            Price::from_str("0.55").expect("price"),
            Count::from_str("1").expect("count"),
            "-0.55".parse().expect("revenue"),
            BalancePrecision::Cent,
            &mut accumulator,
        )
        .expect("fill fee");
    // model = 0.07 * 0.55 * 0.45 = 0.017325; exact = -0.567325; aligned = -0.57;
    // rounding = 0.002675; net = 0.017325 + 0.002675 = 0.02.
    assert_eq!(fee.model_fee, "0.017325".parse().expect("decimal"));
    assert_eq!(fee.trade_fee, "0.017325".parse().expect("decimal"));
    assert_eq!(fee.rounding_fee, "0.002675".parse().expect("decimal"));
    assert_eq!(fee.net_fee, "0.02".parse().expect("decimal"));
}
