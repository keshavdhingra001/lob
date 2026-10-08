//! The reference book (D8): the simplest structure that is obviously correct.
//!
//! It's the oracle the fast book (M4) is tested against, so clarity beats speed
//! everywhere in this file.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::book::{check_peak, BookConfig, Level, OrderBook};
use crate::command::{Command, Event, NewOrder, RejectReason, Stp, StpAction, TimeInForce};
use crate::snapshot::{BookState, RestingOrder};
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
struct Resting {
    id: OrderId,
    /// Shown quantity: all of it, unless this is an iceberg.
    qty: Qty,
    /// An iceberg's slice size (D83). `None` for an ordinary order.
    peak: Option<Qty>,
    /// An iceberg's quantity not shown yet; 0 for an ordinary order.
    hidden: Qty,
    /// Remembered so a modify can't turn a post-only order into a taker (D11).
    post_only: bool,
    /// Remembered so a modify that crosses uses the order's own action (D69).
    stp: Option<Stp>,
}

impl Resting {
    /// An order resting `total` open: an iceberg shows up to its peak and hides the rest.
    fn new(id: OrderId, total: Qty, peak: Option<Qty>, post_only: bool, stp: Option<Stp>) -> Self {
        let qty = peak.map_or(total, |peak| peak.min(total));
        Resting {
            id,
            qty,
            peak,
            hidden: Qty(total.0 - qty.0),
            post_only,
            stp,
        }
    }

    /// Shown plus hidden: what a cancel removes and a modify sets.
    fn total(&self) -> Qty {
        Qty(self.qty.0 + self.hidden.0)
    }
}

/// Price -> orders at that price, oldest first.
type Levels = BTreeMap<Price, VecDeque<Resting>>;

pub struct RefBook {
    config: BookConfig,
    bids: Levels,
    asks: Levels,
    /// Where each resting order lives, so cancel and modify can find it.
    resting: HashMap<OrderId, (Side, Price)>,
    /// The highest id accepted this session. A new order must beat it (D30), which also
    /// rules out reusing an id, with no per-id memory.
    last_id: Option<OrderId>,
}

impl Default for RefBook {
    fn default() -> Self {
        Self::with_config(BookConfig::default())
    }
}

impl RefBook {
    pub fn new() -> Self {
        Self::default()
    }

    fn levels(&self, side: Side) -> &Levels {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: Side) -> &mut Levels {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// The best (first-to-match) price on `side`.
    fn best(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.bids.last_key_value().map(|(&p, _)| p),
            Side::Sell => self.asks.first_key_value().map(|(&p, _)| p),
        }
    }

    /// Whether an order on `side` at `price` would trade on arrival.
    fn would_cross(&self, side: Side, price: Price) -> bool {
        self.best(side.opposite())
            .is_some_and(|best| side.crosses(price, best))
    }

    /// Whether matching would fill all of `qty` at prices crossing `limit` (for FOK).
    /// Matching reaches an iceberg's hidden quantity too, slice by slice (D86). Orders of
    /// the taker's STP group never fill it (D71): with cancel-oldest they're skipped, and
    /// with cancel-newest or cancel-both matching stops at the first one. Every slice
    /// replenished before that goes behind it, so of the orders ahead of it at its level,
    /// only the shown quantity counts.
    fn can_fill(&self, side: Side, qty: Qty, limit: Price, stp: Option<Stp>) -> bool {
        let levels = self.levels(side.opposite());
        let crossing: Box<dyn Iterator<Item = (&Price, &VecDeque<Resting>)>> = match side {
            Side::Buy => Box::new(levels.iter()),
            Side::Sell => Box::new(levels.iter().rev()),
        };
        let mut available = 0;
        for (&price, queue) in crossing {
            if !side.crosses(limit, price) {
                break;
            }
            let (mut shown, mut total) = (0, 0);
            for order in queue {
                match Stp::conflict(stp, order.stp) {
                    None => {
                        shown += order.qty.0;
                        total += order.total().0;
                    }
                    Some(StpAction::CancelOldest) => {}
                    Some(StpAction::CancelNewest | StpAction::CancelBoth) => {
                        return available + shown >= qty.0;
                    }
                }
            }
            available += total;
            if available >= qty.0 {
                return true;
            }
        }
        false
    }

    /// A new limit (`limit = Some`) or market (`limit = None`) order.
    fn submit(&mut self, order: NewOrder, out: &mut Vec<Event>) {
        let NewOrder {
            id,
            side,
            qty,
            limit,
            tif,
            peak,
            stp,
        } = order;
        let check = self.config.check(qty, limit).and_then(|()| {
            check_peak(peak, qty, tif)?;
            if self.last_id.is_some_and(|last| id <= last) {
                Err(RejectReason::IdNotIncreasing)
            } else if tif == TimeInForce::PostOnly
                && limit.is_some_and(|price| self.would_cross(side, price))
            {
                Err(RejectReason::WouldCross)
            } else {
                Ok(())
            }
        });
        if let Err(reason) = check {
            out.push(Event::Rejected { id, reason });
            return;
        }
        self.last_id = Some(id);
        out.push(Event::Accepted { id });

        if tif == TimeInForce::Fok {
            let limit = limit.expect("only limit orders carry a time in force");
            if !self.can_fill(side, qty, limit, stp) {
                out.push(Event::Cancelled { id, remaining: qty });
                return;
            }
        }
        let remaining = self.take(id, side, qty, limit, stp, out);
        if remaining.0 == 0 {
            return;
        }
        match (limit, tif) {
            (Some(price), TimeInForce::Gtc | TimeInForce::PostOnly) => {
                let post_only = tif == TimeInForce::PostOnly;
                self.rest(
                    side,
                    price,
                    Resting::new(id, remaining, peak, post_only, stp),
                )
            }
            // Market and IOC orders never rest. (A FOK order that passed `can_fill` filled
            // completely, so it never gets here.)
            _ => out.push(Event::Cancelled { id, remaining }),
        }
    }

    fn rest(&mut self, side: Side, price: Price, order: Resting) {
        self.levels_mut(side)
            .entry(price)
            .or_default()
            .push_back(order);
        self.resting.insert(order.id, (side, price));
    }

    /// Match `qty` against the opposite side: best price first, oldest order first
    /// within a price, for as long as the price crosses `limit` (`None`: any price).
    /// Every fill trades at the resting (maker) order's price. Returns what's left: zero
    /// if self-trade prevention cancelled the taker (D70), which has then been reported.
    fn take(
        &mut self,
        taker: OrderId,
        side: Side,
        mut qty: Qty,
        limit: Option<Price>,
        stp: Option<Stp>,
        out: &mut Vec<Event>,
    ) -> Qty {
        // Borrow the opposite side and the index as separate fields, so both can change.
        let book = match side {
            Side::Buy => &mut self.asks,
            Side::Sell => &mut self.bids,
        };
        while qty.0 > 0 {
            // Best ask is the lowest price; best bid is the highest.
            let best = match side {
                Side::Buy => book.first_entry(),
                Side::Sell => book.last_entry(),
            };
            let Some(mut level) = best else { break };
            let price = *level.key();
            if limit.is_some_and(|limit| !side.crosses(limit, price)) {
                break;
            }
            let queue = level.get_mut();
            while qty.0 > 0 {
                let Some(maker) = queue.front_mut() else {
                    break;
                };
                if let Some(action) = Stp::conflict(stp, maker.stp) {
                    // Same group: cancel instead of trading. The resting order goes first.
                    if action != StpAction::CancelNewest {
                        // The whole iceberg goes, hidden quantity too (D86).
                        out.push(Event::SelfTradeCancelled {
                            id: maker.id,
                            remaining: maker.total(),
                        });
                        let id = maker.id;
                        queue.pop_front();
                        self.resting.remove(&id);
                    }
                    if action != StpAction::CancelOldest {
                        out.push(Event::SelfTradeCancelled {
                            id: taker,
                            remaining: qty,
                        });
                        qty = Qty(0);
                    }
                    continue;
                }
                let fill = qty.0.min(maker.qty.0);
                out.push(Event::Trade {
                    taker,
                    maker: maker.id,
                    taker_side: side,
                    qty: Qty(fill),
                    price,
                });
                qty.0 -= fill;
                maker.qty.0 -= fill;
                if maker.qty.0 > 0 {
                    continue;
                }
                let mut maker = queue.pop_front().expect("the maker is at the front");
                if maker.hidden.0 == 0 {
                    self.resting.remove(&maker.id);
                    continue;
                }
                // An iceberg shows its next slice at the back of the level (D84), where
                // this sweep may reach it again (D85).
                let peak = maker.peak.expect("only an iceberg hides quantity");
                maker.qty = peak.min(maker.hidden);
                maker.hidden.0 -= maker.qty.0;
                out.push(Event::Replenished {
                    id: maker.id,
                    qty: maker.qty,
                });
                queue.push_back(maker);
            }
            if queue.is_empty() {
                level.remove();
            }
        }
        qty
    }

    /// Take a resting order out of its level, removing the level if it empties.
    fn unlink(&mut self, id: OrderId) -> Option<(Side, Price, Resting)> {
        let (side, price) = self.resting.remove(&id)?;
        let levels = self.levels_mut(side);
        let queue = levels
            .get_mut(&price)
            .expect("indexed order's level exists");
        // O(orders at this price). The fast book (M4) makes this O(1).
        let pos = queue
            .iter()
            .position(|o| o.id == id)
            .expect("indexed order is in its level");
        let order = queue.remove(pos).expect("position is in range");
        if queue.is_empty() {
            levels.remove(&price);
        }
        Some((side, price, order))
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        // Filled, cancelled and never-seen ids all land here: none of them is resting.
        match self.unlink(id) {
            Some((_, _, order)) => out.push(Event::Cancelled {
                id,
                remaining: order.total(),
            }),
            None => out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            }),
        }
    }

    /// D11: same price and no more quantity keeps queue priority; anything else
    /// re-enters the order at the back of its new level, and it may trade on the way.
    fn modify(&mut self, id: OrderId, qty: Qty, price: Price, out: &mut Vec<Event>) {
        let reject = |out: &mut Vec<Event>, reason| out.push(Event::Rejected { id, reason });
        let Some(&(side, old_price)) = self.resting.get(&id) else {
            return reject(out, RejectReason::UnknownOrder);
        };
        if let Err(reason) = self.config.check(qty, Some(price)) {
            return reject(out, reason);
        }
        let crosses = self.would_cross(side, price);
        let queue = self
            .levels_mut(side)
            .get_mut(&old_price)
            .expect("indexed order's level exists");
        let order = queue
            .iter_mut()
            .find(|o| o.id == id)
            .expect("indexed order is in its level");

        if price == old_price && qty <= order.total() {
            // Reducing in place: nobody behind this order is worse off, so it keeps its spot.
            // An iceberg loses hidden quantity first, then shown (D88).
            let cut = order.total().0 - qty.0;
            let from_hidden = cut.min(order.hidden.0);
            order.hidden.0 -= from_hidden;
            order.qty.0 -= cut - from_hidden;
            out.push(Event::Modified { id, qty, price });
            return;
        }
        if order.post_only && crosses {
            return reject(out, RejectReason::WouldCross);
        }
        let (_, _, order) = self.unlink(id).expect("order is resting");
        out.push(Event::Modified { id, qty, price });
        // The order is off the book now, so it can't meet itself; its own action applies.
        let remaining = self.take(id, side, qty, Some(price), order.stp, out);
        if remaining.0 > 0 {
            let order = Resting::new(id, remaining, order.peak, order.post_only, order.stp);
            self.rest(side, price, order);
        }
    }
}

impl OrderBook for RefBook {
    fn with_config(config: BookConfig) -> Self {
        config.validate();
        RefBook {
            config,
            bids: Levels::new(),
            asks: Levels::new(),
            resting: HashMap::new(),
            last_id: None,
        }
    }

    fn apply(&mut self, cmd: &Command, out: &mut Vec<Event>) {
        match *cmd {
            Command::Limit { .. } | Command::Market { .. } => {
                self.submit(cmd.new_order().expect("a new order"), out)
            }
            Command::Modify { id, qty, price } => self.modify(id, qty, price, out),
            Command::Cancel { id } => self.cancel(id, out),
        }
    }

    fn depth(&self, side: Side, n: usize) -> Vec<Level> {
        let level = |(&price, queue): (&Price, &VecDeque<Resting>)| Level {
            price,
            qty: Qty(queue.iter().map(|o| o.qty.0).sum()),
            orders: queue.len(),
        };
        match side {
            Side::Buy => self.bids.iter().rev().take(n).map(level).collect(),
            Side::Sell => self.asks.iter().take(n).map(level).collect(),
        }
    }

    fn check_invariants(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best(Side::Buy), self.best(Side::Sell)) {
            if bid >= ask {
                return Err(format!("crossed book: best bid {bid} >= best ask {ask}"));
            }
        }
        let mut count = 0;
        for side in [Side::Buy, Side::Sell] {
            for (&price, queue) in self.levels(side) {
                if queue.is_empty() {
                    return Err(format!("empty {side} level at {price}"));
                }
                if price.0 % self.config.tick_size != 0 {
                    return Err(format!("{side} level at {price} is off the tick grid"));
                }
                for order in queue {
                    count += 1;
                    if order.qty.0 == 0 {
                        return Err(format!("order {} rests with zero qty", order.id));
                    }
                    if order
                        .peak
                        .map_or(order.hidden.0 > 0, |peak| order.qty > peak)
                    {
                        return Err(format!("order {} shows more than its peak", order.id));
                    }
                    if self.resting.get(&order.id) != Some(&(side, price)) {
                        return Err(format!("order {} missing from the index", order.id));
                    }
                    if self.last_id.is_none_or(|last| order.id > last) {
                        return Err(format!("order {} rests but was never accepted", order.id));
                    }
                }
            }
        }
        if count != self.resting.len() {
            return Err(format!(
                "index has {} orders but the levels hold {count}",
                self.resting.len()
            ));
        }
        Ok(())
    }

    fn state(&self) -> BookState {
        let bids = self.bids.iter().rev().map(|l| (Side::Buy, l));
        let asks = self.asks.iter().map(|l| (Side::Sell, l));
        let orders = bids
            .chain(asks)
            .flat_map(|(side, (&price, queue))| {
                queue.iter().map(move |o| RestingOrder {
                    id: o.id,
                    side,
                    price,
                    qty: o.qty,
                    peak: o.peak,
                    hidden: o.hidden,
                    post_only: o.post_only,
                    stp: o.stp,
                })
            })
            .collect();
        BookState {
            config: self.config,
            last_id: self.last_id,
            orders,
        }
    }

    fn from_state(state: &BookState) -> Result<Self, &'static str> {
        state.validate()?;
        let mut book = RefBook::with_config(state.config);
        book.last_id = state.last_id;
        // Validated order is priority order, so appending rebuilds each queue as it was.
        for o in &state.orders {
            let order = Resting {
                id: o.id,
                qty: o.qty,
                peak: o.peak,
                hidden: o.hidden,
                post_only: o.post_only,
                stp: o.stp,
            };
            book.rest(o.side, o.price, order);
        }
        Ok(book)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(book: &mut RefBook, lines: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for line in lines {
            book.apply(&line.parse().unwrap(), &mut out);
            book.check_invariants().unwrap();
        }
        out.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn filled_orders_leave_the_index() {
        let mut book = RefBook::new();
        run(
            &mut book,
            &[
                "limit 1 sell 10 100",
                "limit 2 sell 10 101",
                "market 3 buy 15",
            ],
        );
        // Order 1 filled completely and is gone; order 2 rests with 5.
        assert_eq!(book.resting.len(), 1);
        assert_eq!(
            book.depth(Side::Sell, 10),
            [Level {
                price: Price(101),
                qty: Qty(5),
                orders: 1,
            }]
        );
        assert_eq!(run(&mut book, &["cancel 1"]), ["rejected 1 unknown-order"]);
    }

    #[test]
    fn depth_is_best_first_and_truncated() {
        let mut book = RefBook::new();
        run(
            &mut book,
            &[
                "limit 1 buy 1 98",
                "limit 2 buy 2 100",
                "limit 3 buy 3 99",
                "limit 4 sell 4 103",
                "limit 5 sell 5 101",
                "limit 6 sell 6 101",
            ],
        );
        let prices =
            |side, n| -> Vec<i64> { book.depth(side, n).iter().map(|l| l.price.0).collect() };
        assert_eq!(prices(Side::Buy, 10), [100, 99, 98]);
        assert_eq!(prices(Side::Sell, 10), [101, 103]);
        assert_eq!(prices(Side::Buy, 2), [100, 99]);
        assert_eq!(book.best_ask().unwrap().qty, Qty(11));
        assert_eq!(book.best_ask().unwrap().orders, 2);
        assert_eq!(book.best_bid().unwrap().price, Price(100));
    }

    #[test]
    fn can_fill_counts_only_crossing_levels() {
        let mut book = RefBook::new();
        run(
            &mut book,
            &[
                "limit 1 sell 5 100",
                "limit 2 sell 5 101",
                "limit 3 sell 5 102",
            ],
        );
        let can_fill = |side, qty, price| book.can_fill(side, Qty(qty), Price(price), None);
        assert!(can_fill(Side::Buy, 10, 101));
        assert!(!can_fill(Side::Buy, 11, 101));
        assert!(can_fill(Side::Buy, 15, 500));
        assert!(!can_fill(Side::Buy, 1, 99));
        assert!(!can_fill(Side::Sell, 1, 1));
    }

    #[test]
    fn invariant_checker_catches_a_crossed_book() {
        let mut book = RefBook::new();
        // Bypass matching to build a book that apply() could never produce.
        for (id, side, price) in [(1, Side::Buy, 101), (2, Side::Sell, 100)] {
            let order = Resting::new(OrderId(id), Qty(1), None, false, None);
            book.rest(side, Price(price), order);
            book.last_id = Some(OrderId(id));
        }
        assert!(book.check_invariants().unwrap_err().contains("crossed"));
    }

    #[test]
    fn invariant_checker_catches_a_stale_index() {
        let mut book = RefBook::new();
        run(&mut book, &["limit 1 buy 5 100"]);
        book.resting.insert(OrderId(9), (Side::Buy, Price(100)));
        assert!(book.check_invariants().is_err());
    }
}
