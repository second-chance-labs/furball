use std::num::NonZeroU32;

use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

/// Token cost of an ordinary read.
pub const DEFAULT_COST: u32 = 10;
/// Token cost of a CF Benchmarks passthrough call.
pub const CF_BENCHMARKS_COST: u32 = 50;
/// Token cost of `GET /portfolio/orders/{order_id}`.
pub const GET_ORDER_COST: u32 = 2;

/// Kalshi account tier; selects the Read-bucket budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// 200 tokens/s.
    #[default]
    Basic,
    /// 300 tokens/s.
    Advanced,
    /// 600 tokens/s.
    Expert,
    /// 1,200 tokens/s.
    Premier,
    /// 2,400 tokens/s.
    Paragon,
    /// 4,800 tokens/s.
    Prime,
    /// 12,000 tokens/s.
    Prestige,
}

impl Tier {
    /// Tokens refilled per second in the Read bucket.
    pub fn read_budget(self) -> u32 {
        match self {
            Self::Basic => 200,
            Self::Advanced => 300,
            Self::Expert => 600,
            Self::Premier => 1_200,
            Self::Paragon => 2_400,
            Self::Prime => 4_800,
            Self::Prestige => 12_000,
        }
    }

    /// Maximum tokens the Read bucket holds: three seconds of budget on Basic and Advanced, one second above.
    pub fn read_capacity(self) -> u32 {
        match self {
            Self::Basic | Self::Advanced => self.read_budget() * 3,
            _ => self.read_budget(),
        }
    }

    /// Balance requests per second the portfolio balance bucket allows; Basic and Advanced are
    /// limited to one, all higher tiers to five.
    pub fn balance_budget(self) -> u32 {
        match self {
            Self::Basic | Self::Advanced => 1,
            _ => 5,
        }
    }
}

/// Token bucket for the Read budget. One governor cell equals one Kalshi token.
pub(crate) struct ReadBucket {
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    capacity: u32,
}

impl ReadBucket {
    pub(crate) fn new(budget_per_second: u32, capacity: u32) -> Self {
        let budget = NonZeroU32::new(budget_per_second.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(capacity.max(1)).unwrap_or(NonZeroU32::MIN);
        Self {
            limiter: RateLimiter::direct(Quota::per_second(budget).allow_burst(burst)),
            capacity: burst.get(),
        }
    }

    /// Waits until `cost` tokens are available and spends them.
    pub(crate) async fn acquire(&self, cost: u32) {
        let cost = NonZeroU32::new(cost.min(self.capacity)).unwrap_or(NonZeroU32::MIN);
        if self.limiter.until_n_ready(cost).await.is_err() {
            tracing::error!("kalshi read bucket cannot cover request cost");
        }
    }
}

/// Token bucket for `GET /portfolio/balance`, separate from the Read budget. One governor cell
/// equals one balance request, and the bucket holds one second of budget, so there is no burst.
pub(crate) struct BalanceBucket {
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

impl BalanceBucket {
    pub(crate) fn new(per_second: u32) -> Self {
        let budget = NonZeroU32::new(per_second.max(1)).unwrap_or(NonZeroU32::MIN);
        Self {
            limiter: RateLimiter::direct(Quota::per_second(budget).allow_burst(budget)),
        }
    }

    /// Waits until one balance request slot is free and spends it.
    pub(crate) async fn acquire(&self) {
        if self.limiter.until_n_ready(NonZeroU32::MIN).await.is_err() {
            tracing::error!("kalshi balance bucket cannot cover request");
        }
    }
}
