//! Computes Kalshi's fee for a taker fill of 100 contracts at $0.55 on a quadratic-fee series.
//! Makes no network call.

use furball::types::FeeType;
use furball::{
    BalancePrecision, Count, FeeAccumulator, FeeCalculator, FeeCoefficients, FeeSide, Price,
};
use rust_decimal::Decimal;

fn main() {
    let calculator = FeeCalculator::new(FeeCoefficients::default());
    let price = Price::new("0.55".parse().expect("price")).expect("price in (0, 1)");
    let count = Count::from(100);
    let revenue = -(price.as_dollars() * count.as_contracts());

    let mut accumulator = FeeAccumulator::new(BalancePrecision::Cent);
    let fee = calculator
        .fill_fee(
            FeeType::Quadratic,
            Decimal::ONE,
            FeeSide::Taker,
            price,
            count,
            revenue,
            BalancePrecision::Cent,
            &mut accumulator,
        )
        .expect("quadratic fees have a documented formula");

    println!("model fee    {}", fee.model_fee);
    println!("trade fee    {}", fee.trade_fee);
    println!("rounding fee {}", fee.rounding_fee);
    println!("net fee      {}", fee.net_fee);
}
