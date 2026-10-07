//! Every symbol's visible NASDAQ book, rebuilt from ITCH order messages (D37, D38).
//!
//! This replays what the exchange reports; it doesn't match. Orders are keyed by NASDAQ's
//! order reference number, which is unique for the day across all symbols. Each side of
//! each symbol keeps its levels in a `BTreeMap<price, Level>`: total shares and order count
//! at the price, which is all a depth view needs (ITCH names the order in every execution,
//! so the queue order within a level never has to be modelled).
//!
//! Two kinds of checks run while replaying:
//! - **Hard errors** ([`BookError`]): a message that refers to an order we don't have, adds
//!   one we already have, takes more shares than are left, or names a different symbol than
//!   the order's. Any of these means the parser or the book is wrong, so replay stops.
//! - **Counted observations** ([`Stats`]): books left crossed or locked, and executions not
//!   at the best price on their side. These depend on market rules (auctions, halts), so
//!   they're counted by phase rather than assumed to be zero.
//!
//! Determinism (D4): the order index is a `HashMap`, but nothing iterates it except
//! `check_invariants`, which sorts what it collects. Symbols are a `Vec` indexed by locate.

use std::collections::{BTreeMap, HashMap};

use thiserror::Error;

use crate::hash::IdBuildHasher;
use crate::itch::{Body, Message, Stock};
use crate::types::Side;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BookError {
    #[error("order {0} isn't on the book")]
    UnknownOrder(u64),
    #[error("order {0} is already on the book")]
    DuplicateOrder(u64),
    #[error("order {order_ref} has {left} shares, message takes {taken}")]
    Overfill {
        order_ref: u64,
        left: u32,
        taken: u32,
    },
    #[error("order {order_ref} is for locate {order}, message says {message}")]
    LocateMismatch {
        order_ref: u64,
        order: u16,
        message: u16,
    },
    #[error("locate {0} has no stock directory message")]
    UnknownLocate(u16),
    #[error("order {0} added with 0 shares")]
    ZeroShares(u64),
}

/// The day's phase, from system event messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Before the `S`/`Q` "start of market hours" event (pre-market trading).
    Pre = 0,
    /// From 9:30 to the `S`/`M` "end of market hours" event.
    Market = 1,
    /// After market hours.
    Post = 2,
}

/// Total shares and order count at one price.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Level {
    pub shares: u64,
    pub orders: u32,
}

#[derive(Clone, Copy, Debug)]
struct Resting {
    locate: u16,
    side: Side,
    price: u32,
    shares: u32,
}

pub struct SymbolBook {
    pub stock: Stock,
    /// From `H` messages: T trading, H halted, P paused, Q quotation only. 0 until the first.
    pub state: u8,
    /// Set by this symbol's opening cross print (`Q` with cross type O).
    pub opened: bool,
    /// Between a cross print and the first book message that isn't a `C` execution: the
    /// auction's executions are still arriving. NASDAQ sends the print (and, after a halt,
    /// the state change back to T) before them, so the book is briefly crossed by design.
    pub uncrossing: bool,
    bids: BTreeMap<u32, Level>,
    asks: BTreeMap<u32, Level>,
}

impl SymbolBook {
    fn side(&self, side: Side) -> &BTreeMap<u32, Level> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<u32, Level> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    pub fn best_bid(&self) -> Option<(u32, Level)> {
        self.bids.last_key_value().map(|(&p, &l)| (p, l))
    }

    pub fn best_ask(&self) -> Option<(u32, Level)> {
        self.asks.first_key_value().map(|(&p, &l)| (p, l))
    }

    /// The best price on `side`.
    pub fn best(&self, side: Side) -> Option<u32> {
        match side {
            Side::Buy => self.best_bid(),
            Side::Sell => self.best_ask(),
        }
        .map(|(p, _)| p)
    }

    /// Up to `n` levels from the best price outwards.
    pub fn depth(&self, side: Side, n: usize) -> Vec<(u32, Level)> {
        let levels = self.side(side).iter().map(|(&p, &l)| (p, l));
        match side {
            Side::Buy => levels.rev().take(n).collect(),
            Side::Sell => levels.take(n).collect(),
        }
    }

    pub fn levels(&self) -> usize {
        self.bids.len() + self.asks.len()
    }

    fn add(&mut self, side: Side, price: u32, shares: u32) {
        let level = self.side_mut(side).entry(price).or_default();
        level.shares += u64::from(shares);
        level.orders += 1;
    }

    /// Take `shares` from the level; `gone` when the order itself leaves the book.
    fn take(&mut self, side: Side, price: u32, shares: u32, gone: bool) {
        let levels = self.side_mut(side);
        let level = levels
            .get_mut(&price)
            .expect("a resting order's level exists");
        level.shares -= u64::from(shares);
        if gone {
            level.orders -= 1;
            if level.orders == 0 {
                debug_assert_eq!(level.shares, 0);
                levels.remove(&price);
            }
        }
    }
}

/// A crossed or locked book, kept as an example to look into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crossing {
    pub stock: Stock,
    pub timestamp: u64,
    pub bid: u32,
    pub ask: u32,
}

/// Counted observations, by [`Phase`] where it matters (index `phase as usize`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: u64,
    /// Messages that changed a book (A, F, E, C, X, D, U).
    pub book_messages: u64,
    pub live_orders: usize,
    pub peak_live_orders: usize,
    /// Book changes on a symbol in state T that left it crossed (bid > ask) / locked (bid == ask).
    pub crossed: [u64; 3],
    pub locked: [u64; 3],
    /// The same during market hours, but only after the symbol's opening cross, and not
    /// while a cross is being unwound. These are the ones that would be real anomalies.
    pub crossed_after_open: u64,
    pub locked_after_open: u64,
    /// Crossed or locked books (any phase) while a cross's executions were still arriving.
    pub crossed_while_uncrossing: u64,
    /// The first few `crossed_after_open` / `locked_after_open` books.
    pub examples: Vec<Crossing>,
    /// `E` executions on a symbol in state T: at the best price on the order's side, or not.
    pub executed_at_best: [u64; 3],
    pub executed_not_at_best: [u64; 3],
    /// `C` executions (cross or other price): counted, price not checked.
    pub executed_with_price: u64,
}

const MAX_EXAMPLES: usize = 10;

pub struct ItchBook {
    orders: HashMap<u64, Resting, IdBuildHasher>,
    /// Indexed by locate. `None` until the symbol's stock directory message.
    symbols: Vec<Option<SymbolBook>>,
    phase: Phase,
    stats: Stats,
}

impl Default for ItchBook {
    fn default() -> Self {
        Self::new()
    }
}

impl ItchBook {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Reserve room for `orders` live orders (a full day peaks in the low millions).
    pub fn with_capacity(orders: usize) -> Self {
        ItchBook {
            orders: HashMap::with_capacity_and_hasher(orders, IdBuildHasher::default()),
            symbols: Vec::new(),
            phase: Phase::Pre,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn symbol(&self, locate: u16) -> Option<&SymbolBook> {
        self.symbols.get(locate as usize)?.as_ref()
    }

    /// Every symbol with a directory entry, in locate order.
    pub fn symbols(&self) -> impl Iterator<Item = (u16, &SymbolBook)> {
        self.symbols
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i as u16, s)))
    }

    fn book_mut(&mut self, locate: u16) -> Result<&mut SymbolBook, BookError> {
        self.symbols
            .get_mut(locate as usize)
            .and_then(Option::as_mut)
            .ok_or(BookError::UnknownLocate(locate))
    }

    /// The resting order `order_ref`, checked against the message's locate.
    fn resting(&self, order_ref: u64, locate: u16) -> Result<Resting, BookError> {
        let o = *self
            .orders
            .get(&order_ref)
            .ok_or(BookError::UnknownOrder(order_ref))?;
        if o.locate != locate {
            return Err(BookError::LocateMismatch {
                order_ref,
                order: o.locate,
                message: locate,
            });
        }
        Ok(o)
    }

    fn add(
        &mut self,
        order_ref: u64,
        locate: u16,
        side: Side,
        shares: u32,
        price: u32,
    ) -> Result<(), BookError> {
        if shares == 0 {
            return Err(BookError::ZeroShares(order_ref));
        }
        if self.orders.contains_key(&order_ref) {
            return Err(BookError::DuplicateOrder(order_ref));
        }
        self.book_mut(locate)?.add(side, price, shares);
        self.orders.insert(
            order_ref,
            Resting {
                locate,
                side,
                price,
                shares,
            },
        );
        self.stats.live_orders = self.orders.len();
        self.stats.peak_live_orders = self.stats.peak_live_orders.max(self.orders.len());
        Ok(())
    }

    /// Take `shares` from `order_ref` (all of them for `None`), removing it when none are left.
    fn reduce(
        &mut self,
        order_ref: u64,
        locate: u16,
        shares: Option<u32>,
    ) -> Result<(), BookError> {
        let o = self.resting(order_ref, locate)?;
        let taken = shares.unwrap_or(o.shares);
        if taken > o.shares {
            return Err(BookError::Overfill {
                order_ref,
                left: o.shares,
                taken,
            });
        }
        let left = o.shares - taken;
        self.book_mut(locate)?
            .take(o.side, o.price, taken, left == 0);
        if left == 0 {
            self.orders.remove(&order_ref);
            self.stats.live_orders = self.orders.len();
        } else {
            self.orders
                .get_mut(&order_ref)
                .expect("checked above")
                .shares = left;
        }
        Ok(())
    }

    /// Count `E` executions at, or not at, the best price on the order's side.
    fn note_execution(&mut self, order_ref: u64, locate: u16) -> Result<(), BookError> {
        let o = self.resting(order_ref, locate)?;
        let book = self
            .symbol(locate)
            .ok_or(BookError::UnknownLocate(locate))?;
        if book.state == b'T' {
            let at_best = book.best(o.side) == Some(o.price);
            let p = self.phase as usize;
            if at_best {
                self.stats.executed_at_best[p] += 1;
            } else {
                self.stats.executed_not_at_best[p] += 1;
            }
        }
        Ok(())
    }

    /// After a book change: count it if it left a trading symbol crossed or locked.
    fn note_crossing(&mut self, locate: u16, timestamp: u64) {
        let phase = self.phase;
        let Some(book) = self.symbols[locate as usize].as_ref() else {
            return;
        };
        if book.state != b'T' {
            return;
        }
        let (Some((bid, _)), Some((ask, _))) = (book.best_bid(), book.best_ask()) else {
            return;
        };
        if bid < ask {
            return;
        }
        let uncrossing = book.uncrossing;
        let after_open = phase == Phase::Market && book.opened && !uncrossing;
        let example = Crossing {
            stock: book.stock,
            timestamp,
            bid,
            ask,
        };
        let s = &mut self.stats;
        s.crossed_while_uncrossing += u64::from(uncrossing);
        if bid > ask {
            s.crossed[phase as usize] += 1;
            s.crossed_after_open += u64::from(after_open);
        } else {
            s.locked[phase as usize] += 1;
            s.locked_after_open += u64::from(after_open);
        }
        if after_open && s.examples.len() < MAX_EXAMPLES {
            s.examples.push(example);
        }
    }

    pub fn apply(&mut self, msg: &Message) -> Result<(), BookError> {
        self.stats.messages += 1;
        let locate = msg.header.locate;
        match msg.body {
            Body::SystemEvent { code } => {
                match code {
                    b'Q' => self.phase = Phase::Market,
                    b'M' | b'E' | b'C' => self.phase = Phase::Post,
                    _ => {}
                }
                return Ok(());
            }
            Body::StockDirectory { stock } => {
                let i = locate as usize;
                if self.symbols.len() <= i {
                    self.symbols.resize_with(i + 1, || None);
                }
                self.symbols[i] = Some(SymbolBook {
                    stock,
                    state: 0,
                    opened: false,
                    uncrossing: false,
                    bids: BTreeMap::new(),
                    asks: BTreeMap::new(),
                });
                return Ok(());
            }
            Body::TradingAction { state, .. } => {
                self.book_mut(locate)?.state = state;
                return Ok(());
            }
            Body::CrossTrade { cross_type, .. } => {
                let book = self.book_mut(locate)?;
                book.uncrossing = true;
                if cross_type == b'O' {
                    book.opened = true;
                }
                return Ok(());
            }
            Body::AddOrder {
                order_ref,
                side,
                shares,
                price,
                ..
            } => self.add(order_ref, locate, side, shares, price)?,
            Body::Executed {
                order_ref, shares, ..
            } => {
                self.note_execution(order_ref, locate)?;
                self.reduce(order_ref, locate, Some(shares))?;
            }
            Body::ExecutedWithPrice {
                order_ref, shares, ..
            } => {
                self.stats.executed_with_price += 1;
                self.reduce(order_ref, locate, Some(shares))?;
            }
            Body::Cancel { order_ref, shares } => self.reduce(order_ref, locate, Some(shares))?,
            Body::Delete { order_ref } => self.reduce(order_ref, locate, None)?,
            Body::Replace {
                old_ref,
                new_ref,
                shares,
                price,
            } => {
                // The new order keeps the old one's side (and symbol, from the header).
                let side = self.resting(old_ref, locate)?.side;
                if self.orders.contains_key(&new_ref) {
                    return Err(BookError::DuplicateOrder(new_ref));
                }
                self.reduce(old_ref, locate, None)?;
                self.add(new_ref, locate, side, shares, price)?;
            }
            Body::Trade { .. } | Body::Other(_) => return Ok(()),
        }
        self.stats.book_messages += 1;
        if !matches!(msg.body, Body::ExecutedWithPrice { .. }) {
            self.book_mut(locate)?.uncrossing = false;
        }
        self.note_crossing(locate, msg.header.timestamp);
        Ok(())
    }

    /// Rebuild every level from the order index and compare: shares and order counts
    /// agree, no level is empty, and every order's symbol exists. O(orders), for tests.
    pub fn check_invariants(&self) -> Result<(), String> {
        let mut want: BTreeMap<(u16, bool, u32), Level> = BTreeMap::new();
        for (r, o) in &self.orders {
            if o.shares == 0 {
                return Err(format!("order {r} has 0 shares"));
            }
            if self.symbol(o.locate).is_none() {
                return Err(format!("order {r} is for unknown locate {}", o.locate));
            }
            let l = want
                .entry((o.locate, o.side == Side::Buy, o.price))
                .or_default();
            l.shares += u64::from(o.shares);
            l.orders += 1;
        }
        let mut have = BTreeMap::new();
        for (locate, book) in self.symbols() {
            for (side, levels) in [(true, &book.bids), (false, &book.asks)] {
                for (&price, &level) in levels {
                    if level.orders == 0 || level.shares == 0 {
                        return Err(format!("empty level {price} on locate {locate}"));
                    }
                    have.insert((locate, side, price), level);
                }
            }
        }
        if want != have {
            let diff = want
                .iter()
                .find(|(k, v)| have.get(k) != Some(v))
                .map(|(k, v)| format!("{k:?}: orders say {v:?}, level says {:?}", have.get(k)))
                .unwrap_or_else(|| "a level with no orders".into());
            return Err(diff);
        }
        if self.stats.live_orders != self.orders.len() {
            return Err("live order count is stale".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itch::Header;

    const L: u16 = 3;

    fn msg(locate: u16, body: Body) -> Message {
        Message {
            header: Header {
                locate,
                tracking: 0,
                timestamp: 1,
            },
            body,
        }
    }

    fn add(order_ref: u64, side: Side, shares: u32, price: u32) -> Message {
        msg(
            L,
            Body::AddOrder {
                order_ref,
                side,
                shares,
                stock: Stock::new("TEST"),
                price,
            },
        )
    }

    fn exec(order_ref: u64, shares: u32) -> Message {
        msg(
            L,
            Body::Executed {
                order_ref,
                shares,
                match_number: 0,
            },
        )
    }

    /// A book with one symbol at locate `L`, trading, in market hours, after its open.
    fn trading() -> ItchBook {
        let mut b = ItchBook::new();
        for m in [
            msg(
                L,
                Body::StockDirectory {
                    stock: Stock::new("TEST"),
                },
            ),
            msg(
                L,
                Body::TradingAction {
                    stock: Stock::new("TEST"),
                    state: b'T',
                },
            ),
            msg(0, Body::SystemEvent { code: b'Q' }),
            msg(
                L,
                Body::CrossTrade {
                    shares: 0,
                    stock: Stock::new("TEST"),
                    price: 0,
                    match_number: 0,
                    cross_type: b'O',
                },
            ),
        ] {
            b.apply(&m).unwrap();
        }
        b
    }

    fn apply_all(b: &mut ItchBook, msgs: &[Message]) {
        for m in msgs {
            b.apply(m).unwrap();
            b.check_invariants().unwrap();
        }
    }

    fn lvl(shares: u64, orders: u32) -> Level {
        Level { shares, orders }
    }

    #[test]
    fn adds_aggregate_into_levels() {
        let mut b = trading();
        apply_all(
            &mut b,
            &[
                add(1, Side::Buy, 100, 1000),
                add(2, Side::Buy, 50, 1000),
                add(3, Side::Buy, 10, 990),
                add(4, Side::Sell, 70, 1010),
            ],
        );
        let s = b.symbol(L).unwrap();
        assert_eq!(
            s.depth(Side::Buy, 5),
            vec![(1000, lvl(150, 2)), (990, lvl(10, 1))]
        );
        assert_eq!(s.best_ask(), Some((1010, lvl(70, 1))));
        assert_eq!(b.stats().live_orders, 4);
    }

    #[test]
    fn executions_cancels_and_deletes_shrink_and_remove() {
        let mut b = trading();
        apply_all(
            &mut b,
            &[
                add(1, Side::Sell, 100, 1010),
                add(2, Side::Sell, 40, 1010),
                exec(1, 30),
                msg(
                    L,
                    Body::Cancel {
                        order_ref: 2,
                        shares: 15,
                    },
                ),
            ],
        );
        assert_eq!(b.symbol(L).unwrap().best_ask(), Some((1010, lvl(95, 2))));
        // Executing the rest removes the order without a delete message.
        apply_all(&mut b, &[exec(1, 70)]);
        assert_eq!(b.symbol(L).unwrap().best_ask(), Some((1010, lvl(25, 1))));
        apply_all(&mut b, &[msg(L, Body::Delete { order_ref: 2 })]);
        assert_eq!(b.symbol(L).unwrap().levels(), 0);
        assert_eq!(b.stats().live_orders, 0);
        assert_eq!(b.stats().peak_live_orders, 2);
        assert!(matches!(
            b.apply(&exec(1, 1)),
            Err(BookError::UnknownOrder(1))
        ));
    }

    #[test]
    fn replace_moves_the_order_and_keeps_its_side() {
        let mut b = trading();
        apply_all(
            &mut b,
            &[
                add(1, Side::Buy, 100, 1000),
                msg(
                    L,
                    Body::Replace {
                        old_ref: 1,
                        new_ref: 9,
                        shares: 60,
                        price: 1005,
                    },
                ),
            ],
        );
        let s = b.symbol(L).unwrap();
        assert_eq!(s.depth(Side::Buy, 5), vec![(1005, lvl(60, 1))]);
        assert_eq!(s.best_ask(), None);
        assert!(matches!(
            b.apply(&exec(1, 1)),
            Err(BookError::UnknownOrder(1))
        ));
        apply_all(&mut b, &[exec(9, 60)]);
        assert_eq!(b.symbol(L).unwrap().levels(), 0);
    }

    #[test]
    fn inconsistent_messages_are_hard_errors() {
        let mut b = trading();
        apply_all(
            &mut b,
            &[add(1, Side::Buy, 100, 1000), add(2, Side::Buy, 5, 999)],
        );
        assert_eq!(
            b.apply(&add(1, Side::Sell, 1, 1)),
            Err(BookError::DuplicateOrder(1))
        );
        assert_eq!(
            b.apply(&exec(1, 101)),
            Err(BookError::Overfill {
                order_ref: 1,
                left: 100,
                taken: 101
            })
        );
        let mut wrong = exec(1, 1);
        wrong.header.locate = L + 1;
        assert_eq!(
            b.apply(&wrong),
            Err(BookError::LocateMismatch {
                order_ref: 1,
                order: L,
                message: L + 1
            })
        );
        let mut nowhere = add(7, Side::Buy, 1, 1);
        nowhere.header.locate = 99;
        assert_eq!(b.apply(&nowhere), Err(BookError::UnknownLocate(99)));
        assert_eq!(
            b.apply(&add(8, Side::Buy, 0, 1)),
            Err(BookError::ZeroShares(8))
        );
        // Replacing onto a live reference fails before the old order is touched.
        let onto_live = msg(
            L,
            Body::Replace {
                old_ref: 1,
                new_ref: 2,
                shares: 1,
                price: 1,
            },
        );
        assert_eq!(b.apply(&onto_live), Err(BookError::DuplicateOrder(2)));
        b.check_invariants().unwrap();
        assert_eq!(b.symbol(L).unwrap().best_bid(), Some((1000, lvl(100, 1))));
    }

    #[test]
    fn counts_executions_away_from_the_best_price() {
        let mut b = trading();
        apply_all(
            &mut b,
            &[
                add(1, Side::Buy, 100, 1000),
                add(2, Side::Buy, 100, 990),
                exec(1, 10),
                exec(2, 10),
            ],
        );
        assert_eq!(b.stats().executed_at_best, [0, 1, 0]);
        assert_eq!(b.stats().executed_not_at_best, [0, 1, 0]);
    }

    #[test]
    fn counts_crossed_and_locked_books_by_phase() {
        let mut b = ItchBook::new();
        let stock = Stock::new("TEST");
        apply_all(
            &mut b,
            &[
                msg(L, Body::StockDirectory { stock }),
                msg(L, Body::TradingAction { stock, state: b'T' }),
                add(1, Side::Buy, 10, 1000),
                add(2, Side::Sell, 10, 1000), // locked, pre-market
            ],
        );
        apply_all(
            &mut b,
            &[
                msg(0, Body::SystemEvent { code: b'Q' }),
                add(3, Side::Sell, 10, 990),
            ],
        ); // crossed, before the open
        assert_eq!(
            (b.stats().locked, b.stats().crossed),
            ([1, 0, 0], [0, 1, 0])
        );
        assert_eq!(b.stats().crossed_after_open, 0);
        assert!(b.stats().examples.is_empty());

        apply_all(
            &mut b,
            &[
                msg(
                    L,
                    Body::CrossTrade {
                        shares: 0,
                        stock,
                        price: 0,
                        match_number: 0,
                        cross_type: b'O',
                    },
                ),
                msg(L, Body::Delete { order_ref: 2 }), // still crossed by order 3
            ],
        );
        assert_eq!(b.stats().crossed_after_open, 1);
        assert_eq!(
            b.stats().examples,
            vec![Crossing {
                stock,
                timestamp: 1,
                bid: 1000,
                ask: 990
            }]
        );

        // The SES sequence from the sample day (D38): a pause leaves the book crossed, the
        // halt cross prints and the state goes back to T, and only then do the `C`
        // executions remove the crossed orders. That window is counted apart.
        let halt_cross = msg(
            L,
            Body::CrossTrade {
                shares: 10,
                stock,
                price: 995,
                match_number: 0,
                cross_type: b'H',
            },
        );
        apply_all(
            &mut b,
            &[
                msg(L, Body::Delete { order_ref: 3 }),
                msg(L, Body::TradingAction { stock, state: b'P' }),
                add(5, Side::Sell, 10, 990),
                add(6, Side::Sell, 10, 995),
                halt_cross,
                msg(L, Body::TradingAction { stock, state: b'T' }),
                msg(
                    L,
                    Body::ExecutedWithPrice {
                        order_ref: 1,
                        shares: 5,
                        match_number: 0,
                        printable: false,
                        price: 995,
                    },
                ),
            ],
        );
        assert_eq!(b.stats().crossed_while_uncrossing, 1);
        assert_eq!(b.stats().crossed_after_open, 1, "unchanged");
        // The first other book message ends the window: still crossed now counts.
        apply_all(&mut b, &[msg(L, Body::Delete { order_ref: 6 })]);
        assert_eq!(b.stats().crossed_while_uncrossing, 1);
        assert_eq!(b.stats().crossed_after_open, 2);
        apply_all(&mut b, &[msg(L, Body::Delete { order_ref: 5 })]);

        // A halted symbol isn't counted.
        apply_all(
            &mut b,
            &[
                msg(L, Body::TradingAction { stock, state: b'H' }),
                add(4, Side::Sell, 10, 980),
            ],
        );
        assert_eq!(b.stats().crossed, [0, 4, 0]);
    }
}
