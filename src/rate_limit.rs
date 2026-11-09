use std::num::NonZeroU32;

use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::Deserialize;

use crate::error::{KalshiError, Result};

/// Token cost of an ordinary read.
pub const DEFAULT_COST: u32 = 10;
/// Token cost of a CF Benchmarks passthrough call.
pub const CF_BENCHMARKS_COST: u32 = 50;
/// Token cost of `GET /portfolio/orders/{order_id}`.
pub const GET_ORDER_COST: u32 = 2;
/// Token cost per order in a `DELETE /portfolio/events/orders/batched` request.
pub const BATCH_CANCEL_COST: u32 = 2;

/// Kalshi account tier; selects the Read, Write and balance bucket budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// 200 tokens/s read, 100 tokens/s write.
    #[default]
    Basic,
    /// 300 tokens/s read, 300 tokens/s write.
    Advanced,
    /// 600 tokens/s read, 600 tokens/s write.
    Expert,
    /// 1,200 tokens/s read, 1,200 tokens/s write.
    Premier,
    /// 2,400 tokens/s read, 2,400 tokens/s write.
    Paragon,
    /// 4,800 tokens/s read, 4,800 tokens/s write.
    Prime,
    /// 12,000 tokens/s read, 9,600 tokens/s write.
    Prestige,
}

impl Tier {
    /// Parses a Kalshi usage-tier name as returned by `GET /account/limits`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "basic" => Some(Self::Basic),
            "advanced" => Some(Self::Advanced),
            "expert" => Some(Self::Expert),
            "premier" => Some(Self::Premier),
            "paragon" => Some(Self::Paragon),
            "prime" => Some(Self::Prime),
            "prestige" => Some(Self::Prestige),
            _ => None,
        }
    }

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

    /// Tokens refilled per second in the Write bucket.
    pub fn write_budget(self) -> u32 {
        match self {
            Self::Basic => 100,
            Self::Advanced => 300,
            Self::Expert => 600,
            Self::Premier => 1_200,
            Self::Paragon => 2_400,
            Self::Prime => 4_800,
            Self::Prestige => 9_600,
        }
    }

    /// Tokens refilled per second in the balance bucket.
    pub fn balance_budget(self) -> u32 {
        match self {
            Self::Basic | Self::Advanced => 1,
            _ => 5,
        }
    }

    /// Maximum tokens the Read bucket holds: three seconds of budget on Basic and Advanced, one second above.
    pub fn read_capacity(self) -> u32 {
        match self {
            Self::Basic | Self::Advanced => self.read_budget() * 3,
            _ => self.read_budget(),
        }
    }

    /// Maximum tokens the Write bucket holds: one second of budget on Basic, three seconds above.
    pub fn write_capacity(self) -> u32 {
        match self {
            Self::Basic => self.write_budget(),
            _ => self.write_budget() * 3,
        }
    }
}

impl<'de> Deserialize<'de> for Tier {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown usage tier: {name}")))
    }
}

/// Which of the account's independent rate buckets a request draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BucketKind {
    Read,
    Write,
    Balance,
}

/// One token bucket: one governor cell equals one Kalshi token.
pub(crate) struct TokenBucket {
    name: &'static str,
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    capacity: u32,
}

impl TokenBucket {
    pub(crate) fn new(name: &'static str, budget_per_second: u32, capacity: u32) -> Self {
        let budget = NonZeroU32::new(budget_per_second.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(capacity.max(1)).unwrap_or(NonZeroU32::MIN);
        Self {
            name,
            limiter: RateLimiter::direct(Quota::per_second(budget).allow_burst(burst)),
            capacity: burst.get(),
        }
    }

    /// Waits until `cost` tokens are available and spends them. A cost larger than the bucket
    /// capacity is rejected without waiting: a batch must fit in one burst.
    pub(crate) async fn acquire(&self, cost: u32) -> Result<()> {
        if cost > self.capacity {
            return Err(KalshiError::CostExceedsBucket {
                cost,
                capacity: self.capacity,
            });
        }
        let cost = NonZeroU32::new(cost).unwrap_or(NonZeroU32::MIN);
        if self.limiter.until_n_ready(cost).await.is_err() {
            tracing::error!("kalshi {} bucket cannot cover request cost", self.name);
            return Err(KalshiError::CostExceedsBucket {
                cost: cost.get(),
                capacity: self.capacity,
            });
        }
        Ok(())
    }
}
