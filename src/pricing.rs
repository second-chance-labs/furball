//! Validated market quantities: [`Price`] in dollars and [`Count`] in contracts.
//!
//! Kalshi trades yes contracts for strictly between 0 and 1 dollar and rejects prices that are
//! off the market's tick grid. Building a [`Price`] or [`Count`] runs those checks, so an
//! invalid value cannot reach an order. All arithmetic is [`Decimal`]; no `f64`.

use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::types::Market;

/// Uniform tick scales (decimal places) for the `price_level_structure` names Kalshi reports.
const LINEAR_TICK_SCALES: &[(&str, u32)] = &[
    ("linear_cent", 2),
    ("linear_mill", 3),
    ("linear_millicent", 4),
];

/// The tick size for a uniform structure: 10 to the negative of `scale`.
fn linear_tick(scale: u32) -> Decimal {
    let mut denominator = Decimal::from(1);
    for _ in 0..scale {
        denominator *= Decimal::from(10);
    }
    Decimal::from(1) / denominator
}

/// A contract price in dollars, strictly between 0 and 1.
///
/// The value keeps the scale it was parsed with, so serializing back produces the same text
/// (`"0.5500"` stays `"0.5500"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(Decimal);

impl Price {
    /// Creates a price from a dollar value; errors unless it is strictly between 0 and 1.
    pub fn new(dollars: Decimal) -> Result<Self, PriceError> {
        if dollars > Decimal::new(0, 0) && dollars < Decimal::new(1, 0) {
            Ok(Self(dollars))
        } else {
            Err(PriceError::OutOfRange(dollars))
        }
    }

    /// Creates a price from cents (divided by 100) and checks the (0, 1) dollar range.
    pub fn from_cents(cents: Decimal) -> Result<Self, PriceError> {
        Self::new(cents / Decimal::new(100, 0))
    }

    /// The price in cents (the dollar value times 100).
    pub fn to_cents(self) -> Decimal {
        self.0 * Decimal::new(100, 0)
    }

    /// The underlying dollar value.
    pub fn as_dollars(self) -> Decimal {
        self.0
    }

    /// The tick size in dollars that `market` uses at this price, from the market's
    /// `price_level_structure` (`linear_cent`, `linear_mill`, `linear_millicent`) or, when the
    /// structure is not one of those, from `price_ranges`.
    pub fn tick_size(self, market: &Market) -> Result<Decimal, PriceError> {
        tick_of(market, self.0)
    }

    /// Checks that this price sits on `market`'s tick grid.
    pub fn validate(self, market: &Market) -> Result<(), PriceError> {
        let tick = self.tick_size(market)?;
        if (self.0 % tick).is_zero() {
            Ok(())
        } else {
            Err(PriceError::OffTick {
                ticker: market.ticker.clone(),
                price: self.0,
                tick,
            })
        }
    }
}

/// Why a [`Price`] could not be built or validated.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PriceError {
    /// The dollar value is not strictly between 0 and 1.
    #[error("price {0} must be more than 0 and less than 1 dollar")]
    OutOfRange(Decimal),
    /// The text does not parse as a decimal.
    #[error("invalid price text '{0}'")]
    Parse(String),
    /// The price is not a multiple of the market's tick size at that price.
    #[error("price {price} of market {ticker} is off the tick grid (tick {tick})")]
    OffTick {
        /// Ticker of the market.
        ticker: String,
        /// The rejected price in dollars.
        price: Decimal,
        /// Tick size in dollars.
        tick: Decimal,
    },
    /// The market's `price_level_structure` is not one of the known linear names and the
    /// market reports no `price_ranges`.
    #[error("market {ticker} reports price level structure '{structure}' without price ranges")]
    UnknownStructure {
        /// Ticker of the market.
        ticker: String,
        /// The unrecognized structure name; empty when the field was absent.
        structure: String,
    },
    /// No price range covers the price.
    #[error("no price range of market {ticker} covers price {price}")]
    NoPriceRange {
        /// Ticker of the market.
        ticker: String,
        /// The price in dollars.
        price: Decimal,
    },
    /// A covering price range declares a non-positive tick size.
    #[error("a price range of market {ticker} declares a non-positive tick size {tick}")]
    InvalidTick {
        /// Ticker of the market.
        ticker: String,
        /// The declared tick size in dollars.
        tick: Decimal,
    },
}

/// The tick size in dollars that `market` uses at `price`.
fn tick_of(market: &Market, price: Decimal) -> Result<Decimal, PriceError> {
    let structure = market.price_level_structure.as_deref();
    if let Some((_, scale)) = LINEAR_TICK_SCALES
        .iter()
        .find(|(name, _)| structure.is_some_and(|actual| actual == *name))
    {
        return Ok(linear_tick(*scale));
    }
    let ranges = market
        .price_ranges
        .as_ref()
        .ok_or_else(|| PriceError::UnknownStructure {
            ticker: market.ticker.clone(),
            structure: structure.unwrap_or_default().to_owned(),
        })?;
    match ranges
        .iter()
        .find(|range| price >= range.start && price <= range.end)
    {
        Some(range) if range.step > Decimal::new(0, 0) => Ok(range.step),
        Some(range) => Err(PriceError::InvalidTick {
            ticker: market.ticker.clone(),
            tick: range.step,
        }),
        None => Err(PriceError::NoPriceRange {
            ticker: market.ticker.clone(),
            price,
        }),
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for Price {
    type Err = PriceError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let dollars = text
            .parse::<Decimal>()
            .map_err(|_| PriceError::Parse(text.to_owned()))?;
        Self::new(dollars)
    }
}

impl From<Price> for Decimal {
    fn from(price: Price) -> Self {
        price.0
    }
}

impl Serialize for Price {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Price {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_str(&text).map_err(serde::de::Error::custom)
    }
}

/// A number of contracts, never negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Count(Decimal);

impl Count {
    /// Creates a count from a contract value; errors when it is negative.
    pub fn new(contracts: Decimal) -> Result<Self, CountError> {
        if contracts < Decimal::new(0, 0) {
            Err(CountError::Negative(contracts))
        } else {
            Ok(Self(contracts))
        }
    }

    /// The underlying contract value.
    pub fn as_contracts(self) -> Decimal {
        self.0
    }
}

/// Why a [`Count`] could not be built.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CountError {
    /// The contract value is negative.
    #[error("count {0} must not be negative")]
    Negative(Decimal),
    /// The text does not parse as a decimal.
    #[error("invalid count text '{0}'")]
    Parse(String),
}

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for Count {
    type Err = CountError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let contracts = text
            .parse::<Decimal>()
            .map_err(|_| CountError::Parse(text.to_owned()))?;
        Self::new(contracts)
    }
}

impl From<Count> for Decimal {
    fn from(count: Count) -> Self {
        count.0
    }
}

impl Serialize for Count {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_str(&text).map_err(serde::de::Error::custom)
    }
}

impl From<i32> for Count {
    fn from(contracts: i32) -> Self {
        Self(Decimal::from(contracts))
    }
}

impl From<i64> for Count {
    fn from(contracts: i64) -> Self {
        Self(Decimal::from(contracts))
    }
}

impl From<u8> for Count {
    fn from(contracts: u8) -> Self {
        Self(Decimal::from(contracts))
    }
}

impl From<u16> for Count {
    fn from(contracts: u16) -> Self {
        Self(Decimal::from(contracts))
    }
}

impl From<u32> for Count {
    fn from(contracts: u32) -> Self {
        Self(Decimal::from(contracts))
    }
}

impl From<u64> for Count {
    fn from(contracts: u64) -> Self {
        Self(Decimal::from(contracts))
    }
}
