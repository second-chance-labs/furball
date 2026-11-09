//! Position and profit-and-loss math, computed from fills and markets. No network calls.
//!
//! A fill's [`Fill::outcome_side`] is the direction of the exposure the fill added: a
//! `yes` fill (buy-yes or sell-no) moves the net yes position up and is accounted at the
//! fill's yes price; a `no` fill (buy-no or sell-yes) moves it down at the same yes price.
//! That is Kalshi's netting model — only the net position settles — and it makes the
//! total profit and loss of a closed position exact, face value of no contracts included.
//! All values are [`Decimal`].

use rust_decimal::Decimal;
use thiserror::Error;

use crate::pricing::{Count, Price};
use crate::types::{Fill, Market, MarketResult, OutcomeSide};

/// What went wrong computing a settlement payoff.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SettlementError {
    /// The market has not settled: its result is empty or unknown.
    #[error("market {ticker} has not settled (result '{result}')")]
    NotSettled {
        /// The market ticker.
        ticker: String,
        /// The market's result string.
        result: String,
    },
    /// A scalar market has settled but no settlement value is recorded.
    #[error("scalar market {ticker} has no settlement value")]
    MissingSettlementValue {
        /// The market ticker.
        ticker: String,
    },
    /// The settlement value is outside [0, 1], so the opposite side's payoff would go
    /// negative.
    #[error("settlement value {value} of market {ticker} is outside [0, 1]")]
    ValueOutOfRange {
        /// The market ticker.
        ticker: String,
        /// The recorded settlement value.
        value: Decimal,
    },
}

/// The payoff at settlement, in dollars, for `count` contracts on `side` of `market`.
///
/// A binary market pays $1 per contract on the winning side: the yes side is paid when
/// the result is `yes`, the no side when it is `no`. A scalar market settles its yes side
/// at [`Market::settlement_value_dollars`] `v`, so the yes side is paid `count * v` and
/// the no side `count * (1 - v)` — the complete-market convention the order book's
/// derived asks already assume. Kalshi rounds payouts to whole cents; this returns the
/// exact, unrounded value.
pub fn payoff_at_settlement(
    side: OutcomeSide,
    count: Count,
    market: &Market,
) -> Result<Decimal, SettlementError> {
    let one = Decimal::new(1, 0);
    let yes_value = match MarketResult::from(market.result.clone()) {
        MarketResult::Yes => one,
        MarketResult::No => Decimal::new(0, 0),
        MarketResult::Scalar => market.settlement_value_dollars.ok_or_else(|| {
            SettlementError::MissingSettlementValue {
                ticker: market.ticker.clone(),
            }
        })?,
        MarketResult::Other(result) => {
            return Err(SettlementError::NotSettled {
                ticker: market.ticker.clone(),
                result,
            });
        }
    };
    if yes_value < Decimal::new(0, 0) || yes_value > one {
        return Err(SettlementError::ValueOutOfRange {
            ticker: market.ticker.clone(),
            value: yes_value,
        });
    }
    let per_contract = match side {
        OutcomeSide::Yes => yes_value,
        OutcomeSide::No => one - yes_value,
    };
    Ok(count.as_contracts() * per_contract)
}

/// What went wrong computing position profit and loss.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum PnlError {
    /// Realized profit and loss needs the position closed, and the yes- and no-side fills
    /// do not carry the same number of contracts.
    #[error("position is not closed: {yes} yes-side contracts against {no} no-side contracts")]
    OpenPosition {
        /// Contracts added by yes-side fills (buy-yes or sell-no).
        yes: Decimal,
        /// Contracts added by no-side fills (buy-no or sell-yes).
        no: Decimal,
    },
    /// The fill list has no fills on the requested side.
    #[error("no fills on the {0:?} side")]
    NoFills(OutcomeSide),
}

/// Contracts and cash flow of the fills, in Kalshi's netting model: yes-side totals,
/// no-side totals, and the net cash (entry costs out, exit proceeds in, all fees out).
fn side_totals(fills: &[Fill]) -> (Decimal, Decimal, Decimal) {
    let mut yes = Decimal::new(0, 0);
    let mut no = Decimal::new(0, 0);
    let mut cash = Decimal::new(0, 0);
    for fill in fills {
        let count = fill.count_fp;
        match fill.outcome_side {
            OutcomeSide::Yes => {
                yes += count;
                cash -= count * fill.yes_price_dollars;
            }
            OutcomeSide::No => {
                no += count;
                cash += count * fill.yes_price_dollars;
            }
        }
        cash -= fill.fee_cost;
    }
    (yes, no, cash)
}

/// The realized profit and loss, in dollars, of a closed position: the net cash of its
/// fills minus every fill's fee.
///
/// Fills are accounted in Kalshi's netting model (see the module docs), so the result
/// equals what Kalshi's settlement actually pays a closed position, including the face
/// value of no contracts. The position must be closed: yes-side and no-side fills must
/// carry the same number of contracts, or [`PnlError::OpenPosition`] is returned.
pub fn realized_pnl(fills: &[Fill]) -> Result<Decimal, PnlError> {
    let (yes, no, cash) = side_totals(fills);
    if yes != no {
        return Err(PnlError::OpenPosition { yes, no });
    }
    Ok(cash)
}

/// The profit and loss, in dollars, of the position the fills leave open, marked at
/// `yes_mark` and reduced by every fill's fee.
///
/// The open position is the net yes position (yes-side fills minus no-side fills) valued
/// at the yes mark; a no position is the negative of its yes equivalent, which carries
/// the same equity as marking no contracts at `1 - yes_mark`.
pub fn unrealized_pnl(fills: &[Fill], yes_mark: Price) -> Result<Decimal, PnlError> {
    let (yes, no, cash) = side_totals(fills);
    Ok(cash + (yes - no) * yes_mark.as_dollars())
}

/// The volume-weighted average entry price, in dollars, of the position on `side`.
///
/// Yes entries are priced at the fill's yes price, no entries at the fill's no price.
/// Fills that added exposure to the opposite side are ignored.
pub fn average_entry_price(fills: &[Fill], side: OutcomeSide) -> Result<Decimal, PnlError> {
    let mut cost = Decimal::new(0, 0);
    let mut count = Decimal::new(0, 0);
    for fill in fills {
        let entry_price = match (side, fill.outcome_side) {
            (OutcomeSide::Yes, OutcomeSide::Yes) => fill.yes_price_dollars,
            (OutcomeSide::No, OutcomeSide::No) => fill.no_price_dollars,
            _ => continue,
        };
        cost += fill.count_fp * entry_price;
        count += fill.count_fp;
    }
    if count.is_zero() {
        return Err(PnlError::NoFills(side));
    }
    Ok(cost / count)
}
