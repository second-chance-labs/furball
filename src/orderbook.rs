//! Order book helpers: best quotes on both sides, spread, mid price, depth, and taker
//! fill cost on [`Orderbook`].
//!
//! Kalshi snapshots carry resting bids only. Each side's ask is the complement of the
//! opposite side's best bid: a no bid at `p` is a yes ask at `1 - p`. All arithmetic is
//! [`Decimal`].

use rust_decimal::Decimal;
use thiserror::Error;

use crate::pricing::Count;
use crate::types::{BookSide, Orderbook, PriceLevel};

/// The result of [`Orderbook::cost_to_fill`]: the walk of the book that covered the request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FillCost {
    /// Contracts to be filled; the requested count.
    pub count: Decimal,
    /// Volume-weighted average fill price in dollars.
    pub average_price: Decimal,
    /// Total cost in dollars: the sum of price times contracts over the consumed levels.
    pub total_cost: Decimal,
}

/// Failures of the order book helpers.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum BookError {
    /// The requested fill count must be positive.
    #[error("cost_to_fill requires a positive count, got {0}")]
    NonPositiveCount(Decimal),
    /// The side holds fewer resting contracts than the request.
    #[error("book too thin on side {side:?}: {available} resting, {requested} requested")]
    TooThin {
        /// The side the taker would buy on.
        side: BookSide,
        /// Contracts resting on that side.
        available: Decimal,
        /// Contracts requested.
        requested: Decimal,
    },
}

impl Orderbook {
    /// Best yes bid: the highest resting yes bid. `None` when the yes side is empty.
    pub fn best_yes_bid(&self) -> Option<Decimal> {
        self.yes_dollars
            .iter()
            .map(|level| level.price_dollars)
            .max()
    }

    /// Best yes ask, derived as `1 -` the best no bid. `None` when the no side is empty.
    pub fn best_yes_ask(&self) -> Option<Decimal> {
        self.best_no_bid().map(|bid| Decimal::new(1, 0) - bid)
    }

    /// Best no bid: the highest resting no bid. `None` when the no side is empty.
    pub fn best_no_bid(&self) -> Option<Decimal> {
        self.no_dollars
            .iter()
            .map(|level| level.price_dollars)
            .max()
    }

    /// Best no ask, derived as `1 -` the best yes bid. `None` when the yes side is empty.
    pub fn best_no_ask(&self) -> Option<Decimal> {
        self.best_yes_bid().map(|bid| Decimal::new(1, 0) - bid)
    }

    /// Yes-side spread: best yes ask minus best yes bid. `None` when either quote is missing.
    pub fn yes_spread(&self) -> Option<Decimal> {
        self.best_yes_bid()
            .zip(self.best_yes_ask())
            .map(|(bid, ask)| ask - bid)
    }

    /// No-side spread: best no ask minus best no bid. `None` when either quote is missing.
    pub fn no_spread(&self) -> Option<Decimal> {
        self.best_no_bid()
            .zip(self.best_no_ask())
            .map(|(bid, ask)| ask - bid)
    }

    /// Yes-side mid price: the midpoint of the best yes bid and ask.
    /// `None` when either quote is missing.
    pub fn yes_mid(&self) -> Option<Decimal> {
        self.best_yes_bid()
            .zip(self.best_yes_ask())
            .map(|(bid, ask)| (bid + ask) / Decimal::new(2, 0))
    }

    /// No-side mid price: the midpoint of the best no bid and ask.
    /// `None` when either quote is missing.
    pub fn no_mid(&self) -> Option<Decimal> {
        self.best_no_bid()
            .zip(self.best_no_ask())
            .map(|(bid, ask)| (bid + ask) / Decimal::new(2, 0))
    }

    /// Total contracts resting in the best `levels` levels of the side's book, or all of
    /// them when the book is shallower.
    pub fn depth(&self, side: BookSide, levels: usize) -> Decimal {
        let book = match side {
            BookSide::Bid => &self.yes_dollars,
            BookSide::Ask => &self.no_dollars,
        };
        best_first(book)
            .into_iter()
            .take(levels)
            .map(|level| level.count_fp)
            .sum()
    }

    /// The cost of taking `count` contracts on `side`: [`BookSide::Bid`] buys yes and
    /// consumes no bids as implicit yes asks; [`BookSide::Ask`] buys no and consumes yes
    /// bids as implicit no asks. Returns the volume-weighted average price and the total
    /// cost; fails with [`BookError::TooThin`] when the book cannot cover the request.
    pub fn cost_to_fill(&self, side: BookSide, count: Count) -> Result<FillCost, BookError> {
        let requested = count.as_contracts();
        if requested <= Decimal::new(0, 0) {
            return Err(BookError::NonPositiveCount(requested));
        }
        let asks = match side {
            BookSide::Bid => &self.no_dollars,
            BookSide::Ask => &self.yes_dollars,
        };
        let mut remaining = requested;
        let mut total_cost = Decimal::new(0, 0);
        let mut available = Decimal::new(0, 0);
        for level in best_first(asks) {
            let ask = Decimal::new(1, 0) - level.price_dollars;
            available += level.count_fp;
            let taken = if level.count_fp >= remaining {
                remaining
            } else {
                level.count_fp
            };
            total_cost += ask * taken;
            remaining -= taken;
            if remaining <= Decimal::new(0, 0) {
                return Ok(FillCost {
                    count: requested,
                    average_price: total_cost / requested,
                    total_cost,
                });
            }
        }
        Err(BookError::TooThin {
            side,
            available,
            requested,
        })
    }
}

/// The book's levels with the best price first. Kalshi returns the best price last, but
/// sorting keeps the helpers correct for either ordering.
fn best_first(book: &[PriceLevel]) -> Vec<&PriceLevel> {
    let mut levels: Vec<&PriceLevel> = book.iter().collect();
    levels.sort_by_key(|level| level.price_dollars);
    levels.reverse();
    levels
}
