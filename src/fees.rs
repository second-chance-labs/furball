//! Fee calculator: computes the fee for a fill exactly as Kalshi does.
//!
//! The base coefficients are published at <https://kalshi.com/fee-schedule> (see
//! [`FeeCoefficients::default`]). The fee type and multiplier for a fill come from the fee
//! schedule built by [`crate::build_fee_schedule`], resolved with [`fee_in_force`]. Rounding
//! follows <https://docs.kalshi.com/getting_started/fee_rounding>: the model fee is rounded
//! up to $0.000001, the balance change is aligned to the member's precision, and the
//! per-order overpayment is rebated in precision steps. All values are [`Decimal`].

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use thiserror::Error;

use crate::fee_schedule::FeeScheduleRow;
use crate::pricing::{Count, Price};
use crate::types::FeeType;

/// Environment variable that overrides the taker coefficient.
pub const FEE_TAKER_ENV: &str = "FURBALL_FEE_TAKER";
/// Environment variable that overrides the maker coefficient.
pub const FEE_MAKER_ENV: &str = "FURBALL_FEE_MAKER";
/// Environment variable that overrides the combo maker coefficient.
pub const FEE_COMBO_MAKER_ENV: &str = "FURBALL_FEE_COMBO_MAKER";

/// The side of a fill, for fee purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeSide {
    /// Took liquidity from the book.
    Taker,
    /// Placed liquidity on the book.
    Maker,
}

/// Balance precision that fees align to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalancePrecision {
    /// Non-direct members: cents.
    Cent,
    /// Direct members: $0.0001.
    TenThousandth,
}

impl BalancePrecision {
    /// The precision step in dollars.
    pub fn step(self) -> Decimal {
        match self {
            Self::Cent => Decimal::new(1, 2),
            Self::TenThousandth => Decimal::new(1, 4),
        }
    }
}

/// Fee formula coefficients.
///
/// [`FeeCoefficients::default`] holds the values published at
/// <https://kalshi.com/fee-schedule>. The taker coefficient is confirmed; the maker values
/// are defaults a person must check in a browser before release, because the page is behind
/// a bot check.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeCoefficients {
    /// Taker coefficient (0.07).
    pub taker: Decimal,
    /// Maker coefficient on most markets (0.0175).
    pub maker: Decimal,
    /// Maker coefficient on combo markets: 50% of the taker coefficient (0.035).
    pub combo_maker: Decimal,
    /// When the published values were recorded.
    pub source_date: NaiveDate,
    /// Where the values are published.
    pub source_url: &'static str,
}

impl Default for FeeCoefficients {
    fn default() -> Self {
        Self {
            taker: "0.07".parse().expect("decimal literal"),
            maker: "0.0175".parse().expect("decimal literal"),
            combo_maker: "0.035".parse().expect("decimal literal"),
            source_date: NaiveDate::from_ymd_opt(2026, 11, 9).expect("literal date"),
            source_url: "https://kalshi.com/fee-schedule",
        }
    }
}

/// What went wrong reading coefficients from the environment.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum FeeCoefficientError {
    /// The variable is set but does not parse as a decimal.
    #[error("{variable} is set to '{value}', which is not a decimal")]
    NotADecimal {
        /// The environment variable name.
        variable: &'static str,
        /// The offending value.
        value: String,
    },
}

impl FeeCoefficients {
    /// Reads the coefficients from `FURBALL_FEE_TAKER`, `FURBALL_FEE_MAKER` and
    /// `FURBALL_FEE_COMBO_MAKER`. A variable that is not set keeps the default value; a
    /// variable set to a value that is not a decimal is an error. This method never reads
    /// `.env` files; an application that wants one loads it before calling this.
    pub fn from_env() -> Result<Self, FeeCoefficientError> {
        let default = Self::default();
        Ok(Self {
            taker: env_coefficient(FEE_TAKER_ENV, default.taker)?,
            maker: env_coefficient(FEE_MAKER_ENV, default.maker)?,
            combo_maker: env_coefficient(FEE_COMBO_MAKER_ENV, default.combo_maker)?,
            ..default
        })
    }
}

fn env_coefficient(
    variable: &'static str,
    fallback: Decimal,
) -> Result<Decimal, FeeCoefficientError> {
    match std::env::var(variable) {
        Ok(value) => value
            .parse::<Decimal>()
            .map_err(|_| FeeCoefficientError::NotADecimal { variable, value }),
        Err(std::env::VarError::NotPresent) => Ok(fallback),
        Err(std::env::VarError::NotUnicode(value)) => Err(FeeCoefficientError::NotADecimal {
            variable,
            value: value.to_string_lossy().into_owned(),
        }),
    }
}

/// What went wrong computing a fee.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum FeeError {
    /// The fee type's formula is not documented, so the fee cannot be computed.
    #[error("fee type '{0}' has no documented formula")]
    UndocumentedFormula(String),
    /// No fee row of the series covers the time.
    #[error("no fee row for series {series} covers {at}")]
    NoFeeRow {
        /// The series ticker.
        series: String,
        /// The fill time.
        at: DateTime<Utc>,
    },
}

/// The fee type and multiplier in force at a point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledFee {
    /// The formula in force.
    pub fee_type: FeeType,
    /// The multiplier in force.
    pub multiplier: Decimal,
}

/// Resolves the fee in force for a series, optionally layered with one event's override,
/// at `at`, from rows produced by [`crate::build_fee_schedule`]. Override fields that are
/// set win; fields that are `None` inherit the series row.
pub fn fee_in_force(
    rows: &[FeeScheduleRow],
    series: &str,
    event: Option<&str>,
    at: DateTime<Utc>,
) -> Result<ScheduledFee, FeeError> {
    let series_row = rows
        .iter()
        .find(|row| {
            row.event_ticker.is_none()
                && row.series_ticker == series
                && covers(row.effective_from, row.effective_to, at)
        })
        .ok_or_else(|| FeeError::NoFeeRow {
            series: series.to_owned(),
            at,
        })?;
    let mut fee_type = series_row.fee_type.clone();
    let mut multiplier = series_row.fee_multiplier;
    if let Some(event) = event
        && let Some(override_row) = rows.iter().find(|row| {
            row.event_ticker.as_deref() == Some(event)
                && row.series_ticker == series
                && covers(row.effective_from, row.effective_to, at)
        })
    {
        if let Some(override_fee_type) = &override_row.fee_type {
            fee_type = Some(override_fee_type.clone());
        }
        if let Some(override_multiplier) = override_row.fee_multiplier {
            multiplier = Some(override_multiplier);
        }
    }
    match (fee_type, multiplier) {
        (Some(fee_type), Some(multiplier)) => Ok(ScheduledFee {
            fee_type: fee_type.into(),
            multiplier,
        }),
        _ => Err(FeeError::NoFeeRow {
            series: series.to_owned(),
            at,
        }),
    }
}

/// Whether `at` falls in the half-open interval `[from, to)`.
fn covers(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>, at: DateTime<Utc>) -> bool {
    from.map(|from| at >= from).unwrap_or(true) && to.map(|to| at < to).unwrap_or(true)
}

/// Per-order fee accumulator that rebates rounding overpayment in the member's balance
/// precision steps. Create one per order and hand it to [`FeeCalculator::fill_fee`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeeAccumulator {
    precision: BalancePrecision,
    carried: Decimal,
}

impl FeeAccumulator {
    /// Starts a fresh accumulator for one order at the member's precision.
    pub fn new(precision: BalancePrecision) -> Self {
        Self {
            precision,
            carried: Decimal::new(0, 0),
        }
    }

    /// The overpayment carried between fills, after the last rebate.
    pub fn carried(&self) -> Decimal {
        self.carried
    }

    /// Adds one fill's rounding overpayment and rebates the largest multiple of the
    /// precision that has accumulated. Returns the rebate.
    pub fn add_rounding(&mut self, rounding_fee: Decimal) -> Decimal {
        self.carried += rounding_fee;
        let step = self.precision.step();
        let rebate = (self.carried / step).trunc() * step;
        self.carried -= rebate;
        rebate
    }
}

/// One fill's fee, broken down as in Kalshi's fee rounding docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeeBreakdown {
    /// The formula's output before rounding.
    pub model_fee: Decimal,
    /// The model fee rounded up to $0.000001.
    pub trade_fee: Decimal,
    /// The alignment adjustment that puts the balance change back on the precision grid.
    pub rounding_fee: Decimal,
    /// The overpayment rebated from the order accumulator.
    pub rebate: Decimal,
    /// `trade_fee + rounding_fee - rebate`, floored at zero: the fill's net fee never
    /// goes negative.
    pub net_fee: Decimal,
}

/// Computes fees exactly as Kalshi does.
pub struct FeeCalculator {
    coefficients: FeeCoefficients,
}

impl FeeCalculator {
    /// Uses `coefficients` for every fee it computes.
    pub fn new(coefficients: FeeCoefficients) -> Self {
        Self { coefficients }
    }

    /// The coefficients in use.
    pub fn coefficients(&self) -> &FeeCoefficients {
        &self.coefficients
    }

    /// The model fee, before rounding, for one fill: the formula chosen by `fee_type`,
    /// scaled by `multiplier`. The `quadratic` family is `multiplier * coefficient *
    /// price * (1 - price) * count`; `quadratic` has no maker fee,
    /// `quadratic_with_maker_fees` charges makers, and
    /// `quadratic_with_combo_maker_fees` charges them the combo coefficient. `flat` and
    /// unknown fee types have no documented formula and error with
    /// [`FeeError::UndocumentedFormula`].
    pub fn model_fee(
        &self,
        fee_type: FeeType,
        multiplier: Decimal,
        side: FeeSide,
        price: Price,
        count: Count,
    ) -> Result<Decimal, FeeError> {
        let coefficient = match (fee_type, side) {
            (FeeType::Quadratic, FeeSide::Taker) => self.coefficients.taker,
            (FeeType::Quadratic, FeeSide::Maker) => return Ok(Decimal::new(0, 0)),
            (FeeType::QuadraticWithMakerFees, FeeSide::Taker) => self.coefficients.taker,
            (FeeType::QuadraticWithMakerFees, FeeSide::Maker) => self.coefficients.maker,
            (FeeType::QuadraticWithComboMakerFees, FeeSide::Taker) => self.coefficients.taker,
            (FeeType::QuadraticWithComboMakerFees, FeeSide::Maker) => self.coefficients.combo_maker,
            other => return Err(FeeError::UndocumentedFormula(other.0.into())),
        };
        let p = price.as_dollars();
        Ok(multiplier * coefficient * p * (Decimal::new(1, 0) - p) * count.as_contracts())
    }

    /// The fee for one fill, with all of Kalshi's rounding applied. `model_fee` is the
    /// formula's output, `revenue` the balance change of the trade itself before fees
    /// (negative for a buy, positive for a sell), and `accumulator` the per-order state.
    /// The trade fee is the model fee rounded up to $0.000001; the rounding fee aligns
    /// `revenue - trade_fee` to the member's precision; the rebate is the overpayment
    /// refunded by the accumulator.
    pub fn round_fill(
        &self,
        model_fee: Decimal,
        revenue: Decimal,
        precision: BalancePrecision,
        accumulator: &mut FeeAccumulator,
    ) -> FeeBreakdown {
        let trade_fee = ceil_to_step(model_fee, Decimal::new(1, 6));
        let exact = revenue - trade_fee;
        let aligned = floor_to_step(exact, precision.step());
        let rounding_fee = exact - aligned;
        let rebate = accumulator.add_rounding(rounding_fee);
        let before_cap = trade_fee + rounding_fee - rebate;
        let net_fee = if before_cap < Decimal::new(0, 0) {
            Decimal::new(0, 0)
        } else {
            before_cap
        };
        FeeBreakdown {
            model_fee,
            trade_fee,
            rounding_fee,
            rebate,
            net_fee,
        }
    }

    /// The fee for one fill from the formula and all of Kalshi's rounding:
    /// [`Self::model_fee`] followed by [`Self::round_fill`].
    #[allow(
        clippy::too_many_arguments,
        reason = "each input is an independent dimension of the fill"
    )]
    pub fn fill_fee(
        &self,
        fee_type: FeeType,
        multiplier: Decimal,
        side: FeeSide,
        price: Price,
        count: Count,
        revenue: Decimal,
        precision: BalancePrecision,
        accumulator: &mut FeeAccumulator,
    ) -> Result<FeeBreakdown, FeeError> {
        let model_fee = self.model_fee(fee_type, multiplier, side, price, count)?;
        Ok(self.round_fill(model_fee, revenue, precision, accumulator))
    }
}

/// Rounds a non-negative value up to the next multiple of `step`.
fn ceil_to_step(value: Decimal, step: Decimal) -> Decimal {
    let units = value / step;
    let whole = units.trunc();
    if (units - whole).is_zero() {
        whole * step
    } else {
        (whole + Decimal::new(1, 0)) * step
    }
}

/// Rounds a value down to the previous multiple of `step`, toward negative infinity.
fn floor_to_step(value: Decimal, step: Decimal) -> Decimal {
    let units = value / step;
    let whole = units.trunc();
    if (units - whole).is_zero() || value > Decimal::new(0, 0) {
        whole * step
    } else {
        (whole - Decimal::new(1, 0)) * step
    }
}
