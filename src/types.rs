//! Core value types. Prices and quantities are integers (D2): no floating point
//! anywhere on the matching path.

use std::fmt;

/// A price in integer ticks. With a $0.01 tick, $100.25 is `Price(10025)`.
///
/// Signed because some instruments (spreads, some futures) trade at negative prices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub i64);

/// A quantity in whole units (shares, contracts, lots).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Qty(pub u64);

/// Client-assigned order id, unique per session (D3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// The side this order matches against.
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// Whether an order on this side at `limit` is willing to trade at `resting`.
    /// A buy crosses any ask at or below its limit; a sell crosses any bid at or above it.
    pub fn crosses(self, limit: Price, resting: Price) -> bool {
        match self {
            Side::Buy => resting <= limit,
            Side::Sell => resting >= limit,
        }
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposite_flips() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn buy_crosses_asks_at_or_below_limit() {
        assert!(Side::Buy.crosses(Price(100), Price(99)));
        assert!(Side::Buy.crosses(Price(100), Price(100)));
        assert!(!Side::Buy.crosses(Price(100), Price(101)));
    }

    #[test]
    fn sell_crosses_bids_at_or_above_limit() {
        assert!(Side::Sell.crosses(Price(100), Price(101)));
        assert!(Side::Sell.crosses(Price(100), Price(100)));
        assert!(!Side::Sell.crosses(Price(100), Price(99)));
    }

    #[test]
    fn negative_prices_order_correctly() {
        assert!(Price(-5) < Price(0));
        assert!(Side::Buy.crosses(Price(-3), Price(-5)));
    }
}
