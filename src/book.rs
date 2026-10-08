//! The interface every book implementation provides. M1 adds the reference book
//! (simple, obviously correct); M4 adds the fast book and tests it against M1.

use crate::command::{Command, Event, RejectReason};
use crate::snapshot::BookState;
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

/// Per-instrument rules the engine enforces (D14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookConfig {
    /// Every limit price must be a multiple of this many ticks. Must be positive.
    pub tick_size: i64,
    /// The largest quantity one order (or a modify) may have. A fat-finger guard.
    pub max_qty: u64,
}

impl Default for BookConfig {
    fn default() -> Self {
        BookConfig {
            tick_size: 1,
            max_qty: 1_000_000,
        }
    }
}

impl BookConfig {
    /// Panics on a config no book could enforce sensibly.
    pub fn validate(&self) {
        assert!(self.tick_size > 0, "tick_size must be positive");
        assert!(self.max_qty > 0, "max_qty must be positive");
    }

    /// The rules every new order and modify must pass (D14). `price` is `None` for a market
    /// order. Both books call this; their matching logic stays separate on purpose (D22).
    pub fn check(&self, qty: Qty, price: Option<Price>) -> Result<(), RejectReason> {
        if qty.0 == 0 {
            Err(RejectReason::ZeroQty)
        } else if qty.0 > self.max_qty {
            Err(RejectReason::QtyTooLarge)
        } else if price.is_some_and(|p| p.0 % self.tick_size != 0) {
            Err(RejectReason::BadTick)
        } else {
            Ok(())
        }
    }
}

/// Apply every command in order, untimed, reusing one event buffer. Returns the number of
/// events, so a caller can keep the work from being optimized away.
pub fn apply_all<B: OrderBook>(book: &mut B, commands: &[Command]) -> usize {
    let mut events = Vec::with_capacity(64);
    let mut total = 0;
    for cmd in commands {
        events.clear();
        book.apply(cmd, &mut events);
        total += events.len();
    }
    total
}

pub trait OrderBook {
    /// An empty book enforcing `config`.
    fn with_config(config: BookConfig) -> Self
    where
        Self: Sized;

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

    /// The logical book, for a snapshot (D74): what any book needs to carry on exactly
    /// where this one is.
    fn state(&self) -> BookState;

    /// A book that behaves exactly like the one `state` was taken from. Refuses a state
    /// no session could have produced (`BookState::validate`).
    fn from_state(state: &BookState) -> Result<Self, &'static str>
    where
        Self: Sized;

    fn best_bid(&self) -> Option<Level> {
        self.depth(Side::Buy, 1).first().copied()
    }

    fn best_ask(&self) -> Option<Level> {
        self.depth(Side::Sell, 1).first().copied()
    }
}
