//! The interface every book implementation provides. M1 adds the reference book
//! (simple, obviously correct); M4 adds the fast book and tests it against M1.

use crate::command::{Command, Event};
use crate::types::{Price, Qty, Side};

/// Aggregated view of one price level, for depth queries and market data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub price: Price,
    /// Total resting quantity at this price.
    pub qty: Qty,
    /// Number of resting orders at this price.
    pub orders: usize,
}

pub trait OrderBook {
    /// Apply one command and append the events it causes to `out`, in order.
    ///
    /// `out` is owned by the caller and not cleared, so a hot loop can reuse one
    /// buffer instead of allocating per command (D5).
    fn apply(&mut self, cmd: &Command, out: &mut Vec<Event>);

    /// Up to `n` levels on `side`, best price first (highest bid, lowest ask).
    fn depth(&self, side: Side, n: usize) -> Vec<Level>;

    /// Check the book's internal consistency: never crossed, no empty levels, no
    /// zero-quantity orders, and lookup structures agree with the queues. Tests call it
    /// after every command. It's O(book size), so it never runs on the hot path.
    fn check_invariants(&self) -> Result<(), String>;

    fn best_bid(&self) -> Option<Level> {
        self.depth(Side::Buy, 1).first().copied()
    }

    fn best_ask(&self) -> Option<Level> {
        self.depth(Side::Sell, 1).first().copied()
    }
}
