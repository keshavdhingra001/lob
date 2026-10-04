//! The fast book (D19–D22): same behaviour as the reference book, event for event,
//! with cheaper data structures.
//!
//! - Orders live in a slab (`Vec` + free list) and are addressed by `u32` index, so
//!   resting an order reuses a slot instead of allocating once the book is warm.
//! - Each price level is an intrusive doubly linked list threaded through the slab
//!   (`prev`/`next`), with head, tail, total quantity and order count kept on the level.
//! - An id -> slot map makes cancel and modify O(1) to find and O(1) to unlink.
//! - The best level on each side is cached; the price tree is only consulted when a
//!   level is created or the best level empties.
//!
//! Correctness is defined by `RefBook`: the differential tests feed both books the same
//! commands and require identical events.

use std::collections::{BTreeMap, HashMap};

use crate::book::{BookConfig, Level, OrderBook};
use crate::command::{Command, Event, RejectReason, TimeInForce};
use crate::types::{OrderId, Price, Qty, Side};

/// "No index": the end of a list, or no best level.
const NIL: u32 = u32::MAX;

/// A `Vec` with a free list. Indices stay valid until removed, then get reused.
struct Slab<T> {
    items: Vec<T>,
    free: Vec<u32>,
}

impl<T> Slab<T> {
    fn new() -> Self {
        Slab {
            items: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, item: T) -> u32 {
        match self.free.pop() {
            Some(i) => {
                self.items[i as usize] = item;
                i
            }
            None => {
                self.items.push(item);
                u32::try_from(self.items.len() - 1).expect("fewer than 2^32 slots")
            }
        }
    }

    /// Marks the slot free. The stale value stays until the slot is reused.
    fn remove(&mut self, i: u32) {
        self.free.push(i);
    }

    fn live(&self) -> usize {
        self.items.len() - self.free.len()
    }
}

impl<T> std::ops::Index<u32> for Slab<T> {
    type Output = T;
    fn index(&self, i: u32) -> &T {
        &self.items[i as usize]
    }
}

impl<T> std::ops::IndexMut<u32> for Slab<T> {
    fn index_mut(&mut self, i: u32) -> &mut T {
        &mut self.items[i as usize]
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    id: OrderId,
    qty: u64,
    /// The level this order rests on.
    level: u32,
    prev: u32,
    next: u32,
    post_only: bool,
}

#[derive(Clone, Copy, Debug)]
struct LevelNode {
    price: Price,
    side: Side,
    head: u32,
    tail: u32,
    /// Sum of `qty` over the list, so depth and FOK checks never walk orders.
    total: u64,
    count: u32,
}

pub struct FastBook {
    config: BookConfig,
    orders: Slab<Node>,
    levels: Slab<LevelNode>,
    /// Price -> level index, per side.
    bids: BTreeMap<Price, u32>,
    asks: BTreeMap<Price, u32>,
    /// Cached best level per side (`NIL` when the side is empty). [bids, asks].
    best: [u32; 2],
    /// Resting order id -> slot.
    index: HashMap<OrderId, u32>,
    /// The highest id accepted this session (D30).
    last_id: Option<OrderId>,
}

fn side_ix(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

impl FastBook {
    pub fn new() -> Self {
        Self::with_config(BookConfig::default())
    }

    fn tree(&self, side: Side) -> &BTreeMap<Price, u32> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn tree_mut(&mut self, side: Side) -> &mut BTreeMap<Price, u32> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// The best level according to the tree (ignoring the cache).
    fn tree_best(&self, side: Side) -> u32 {
        let best = match side {
            Side::Buy => self.bids.last_key_value(),
            Side::Sell => self.asks.first_key_value(),
        };
        best.map_or(NIL, |(_, &l)| l)
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        let l = self.best[side_ix(side)];
        (l != NIL).then(|| self.levels[l].price)
    }

    fn would_cross(&self, side: Side, price: Price) -> bool {
        self.best_price(side.opposite())
            .is_some_and(|best| side.crosses(price, best))
    }

    /// FOK pre-scan: walks levels, not orders, thanks to `total`.
    fn can_fill(&self, side: Side, qty: Qty, limit: Price) -> bool {
        let tree = self.tree(side.opposite());
        let mut available = 0;
        let mut check = |&l: &u32| {
            let level = &self.levels[l];
            if !side.crosses(limit, level.price) {
                return Some(false);
            }
            available += level.total;
            (available >= qty.0).then_some(true)
        };
        let found = match side {
            Side::Buy => tree.values().find_map(&mut check),
            Side::Sell => tree.values().rev().find_map(&mut check),
        };
        found == Some(true)
    }

    fn validate(&self, qty: Qty, price: Option<Price>) -> Result<(), RejectReason> {
        if qty.0 == 0 {
            Err(RejectReason::ZeroQty)
        } else if qty.0 > self.config.max_qty {
            Err(RejectReason::QtyTooLarge)
        } else if price.is_some_and(|p| p.0 % self.config.tick_size != 0) {
            Err(RejectReason::BadTick)
        } else {
            Ok(())
        }
    }

    fn submit(
        &mut self,
        id: OrderId,
        side: Side,
        qty: Qty,
        limit: Option<Price>,
        tif: TimeInForce,
        out: &mut Vec<Event>,
    ) {
        let check = self.validate(qty, limit).and_then(|()| {
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
            if !self.can_fill(side, qty, limit) {
                out.push(Event::Cancelled { id, remaining: qty });
                return;
            }
        }
        let remaining = self.take(id, side, qty.0, limit, out);
        if remaining == 0 {
            return;
        }
        match (limit, tif) {
            (Some(price), TimeInForce::Gtc | TimeInForce::PostOnly) => {
                self.rest(id, side, price, remaining, tif == TimeInForce::PostOnly)
            }
            _ => out.push(Event::Cancelled {
                id,
                remaining: Qty(remaining),
            }),
        }
    }

    /// Append an order to the back of its price level, creating the level if needed.
    fn rest(&mut self, id: OrderId, side: Side, price: Price, qty: u64, post_only: bool) {
        let level = match self.tree(side).get(&price) {
            Some(&l) => l,
            None => self.new_level(side, price),
        };
        let tail = self.levels[level].tail;
        let slot = self.orders.insert(Node {
            id,
            qty,
            level,
            prev: tail,
            next: NIL,
            post_only,
        });
        if tail == NIL {
            self.levels[level].head = slot;
        } else {
            self.orders[tail].next = slot;
        }
        let lv = &mut self.levels[level];
        lv.tail = slot;
        lv.total += qty;
        lv.count += 1;
        self.index.insert(id, slot);
    }

    fn new_level(&mut self, side: Side, price: Price) -> u32 {
        let level = self.levels.insert(LevelNode {
            price,
            side,
            head: NIL,
            tail: NIL,
            total: 0,
            count: 0,
        });
        self.tree_mut(side).insert(price, level);
        // A new level becomes the best if it beats the current best. (Prices in the tree
        // are unique, so it's never equal.)
        let best = self.best[side_ix(side)];
        let better = best == NIL
            || match side {
                Side::Buy => price > self.levels[best].price,
                Side::Sell => price < self.levels[best].price,
            };
        if better {
            self.best[side_ix(side)] = level;
        }
        level
    }

    fn remove_level(&mut self, level: u32) {
        let LevelNode { price, side, .. } = self.levels[level];
        self.tree_mut(side).remove(&price);
        self.levels.remove(level);
        if self.best[side_ix(side)] == level {
            self.best[side_ix(side)] = self.tree_best(side);
        }
    }

    /// Take an order out of its level's list and free its slot. Removes the level if it empties.
    fn unlink(&mut self, slot: u32) -> Node {
        let node = self.orders[slot];
        if node.prev == NIL {
            self.levels[node.level].head = node.next;
        } else {
            self.orders[node.prev].next = node.next;
        }
        if node.next == NIL {
            self.levels[node.level].tail = node.prev;
        } else {
            self.orders[node.next].prev = node.prev;
        }
        let lv = &mut self.levels[node.level];
        lv.total -= node.qty;
        lv.count -= 1;
        let empty = lv.count == 0;
        self.orders.remove(slot);
        if empty {
            self.remove_level(node.level);
        }
        node
    }

    /// Match against the opposite side; see `RefBook::take`. Returns the unfilled quantity.
    fn take(
        &mut self,
        taker: OrderId,
        side: Side,
        mut qty: u64,
        limit: Option<Price>,
        out: &mut Vec<Event>,
    ) -> u64 {
        let opposite = side_ix(side.opposite());
        while qty > 0 {
            let level = self.best[opposite];
            if level == NIL {
                break;
            }
            let price = self.levels[level].price;
            if limit.is_some_and(|limit| !side.crosses(limit, price)) {
                break;
            }
            // Fill from the head of the level. When the level empties, `unlink` removes it
            // and moves `best` on, so the outer loop picks up the next level.
            while qty > 0 && self.best[opposite] == level {
                let slot = self.levels[level].head;
                let maker = &mut self.orders[slot];
                let fill = qty.min(maker.qty);
                maker.qty -= fill;
                let (maker_id, maker_done) = (maker.id, maker.qty == 0);
                out.push(Event::Trade {
                    taker,
                    maker: maker_id,
                    taker_side: side,
                    qty: Qty(fill),
                    price,
                });
                qty -= fill;
                self.levels[level].total -= fill;
                if maker_done {
                    // The node's qty is now 0, so `unlink` leaves the level total alone.
                    self.index.remove(&maker_id);
                    self.unlink(slot);
                }
            }
        }
        qty
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        match self.index.remove(&id) {
            Some(slot) => {
                let node = self.unlink(slot);
                out.push(Event::Cancelled {
                    id,
                    remaining: Qty(node.qty),
                });
            }
            None => out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            }),
        }
    }

    /// See `RefBook::modify` and D11.
    fn modify(&mut self, id: OrderId, qty: Qty, price: Price, out: &mut Vec<Event>) {
        let reject = |out: &mut Vec<Event>, reason| out.push(Event::Rejected { id, reason });
        let Some(&slot) = self.index.get(&id) else {
            return reject(out, RejectReason::UnknownOrder);
        };
        if let Err(reason) = self.validate(qty, Some(price)) {
            return reject(out, reason);
        }
        let node = self.orders[slot];
        let lv = self.levels[node.level];
        if price == lv.price && qty.0 <= node.qty {
            self.levels[node.level].total -= node.qty - qty.0;
            self.orders[slot].qty = qty.0;
            out.push(Event::Modified { id, qty, price });
            return;
        }
        if node.post_only && self.would_cross(lv.side, price) {
            return reject(out, RejectReason::WouldCross);
        }
        self.index.remove(&id);
        self.unlink(slot);
        out.push(Event::Modified { id, qty, price });
        let remaining = self.take(id, lv.side, qty.0, Some(price), out);
        if remaining > 0 {
            self.rest(id, lv.side, price, remaining, node.post_only);
        }
    }
}

impl Default for FastBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook for FastBook {
    fn with_config(config: BookConfig) -> Self {
        config.validate();
        FastBook {
            config,
            orders: Slab::new(),
            levels: Slab::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            best: [NIL, NIL],
            index: HashMap::new(),
            last_id: None,
        }
    }

    fn apply(&mut self, cmd: &Command, out: &mut Vec<Event>) {
        match *cmd {
            Command::Limit {
                id,
                side,
                qty,
                price,
                tif,
            } => self.submit(id, side, qty, Some(price), tif, out),
            Command::Market { id, side, qty } => {
                self.submit(id, side, qty, None, TimeInForce::Gtc, out)
            }
            Command::Modify { id, qty, price } => self.modify(id, qty, price, out),
            Command::Cancel { id } => self.cancel(id, out),
        }
    }

    fn depth(&self, side: Side, n: usize) -> Vec<Level> {
        let level = |&l: &u32| {
            let lv = &self.levels[l];
            Level {
                price: lv.price,
                qty: Qty(lv.total),
                orders: lv.count as usize,
            }
        };
        match side {
            Side::Buy => self.bids.values().rev().take(n).map(level).collect(),
            Side::Sell => self.asks.values().take(n).map(level).collect(),
        }
    }

    fn check_invariants(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best_price(Side::Buy), self.best_price(Side::Sell)) {
            if bid >= ask {
                return Err(format!("crossed book: best bid {bid} >= best ask {ask}"));
            }
        }
        let mut orders = 0;
        for side in [Side::Buy, Side::Sell] {
            if self.best[side_ix(side)] != self.tree_best(side) {
                return Err(format!("stale best-level cache on the {side} side"));
            }
            for (&price, &l) in self.tree(side) {
                let lv = &self.levels[l];
                if lv.price != price || lv.side != side {
                    return Err(format!("level {l} filed under {side} {price} is {lv:?}"));
                }
                if price.0 % self.config.tick_size != 0 {
                    return Err(format!("{side} level at {price} is off the tick grid"));
                }
                // Walk the list forwards, checking back links, ownership and the totals.
                let (mut total, mut count, mut prev, mut slot) = (0, 0, NIL, lv.head);
                while slot != NIL {
                    let node = &self.orders[slot];
                    if node.prev != prev || node.level != l {
                        return Err(format!("broken links at slot {slot} in {side} {price}"));
                    }
                    if node.qty == 0 {
                        return Err(format!("order {} rests with zero qty", node.id));
                    }
                    if self.index.get(&node.id) != Some(&slot)
                        || self.last_id.is_none_or(|last| node.id > last)
                    {
                        return Err(format!("order {} missing from the index", node.id));
                    }
                    total += node.qty;
                    count += 1;
                    prev = slot;
                    slot = node.next;
                    if count > self.orders.items.len() {
                        return Err(format!("cycle in {side} {price}"));
                    }
                }
                if count == 0 || prev != lv.tail || total != lv.total || count != lv.count as usize
                {
                    return Err(format!(
                        "{side} {price}: list has {count} orders / {total} qty, level says {} / {} (tail ok: {})",
                        lv.count,
                        lv.total,
                        prev == lv.tail
                    ));
                }
                orders += count;
            }
        }
        if orders != self.index.len() || orders != self.orders.live() {
            return Err(format!(
                "lists hold {orders} orders, index {}, slab {}",
                self.index.len(),
                self.orders.live()
            ));
        }
        if self.levels.live() != self.bids.len() + self.asks.len() {
            return Err("level slab and price trees disagree".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(book: &mut FastBook, lines: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for line in lines {
            book.apply(&line.parse().unwrap(), &mut out);
            book.check_invariants().unwrap();
        }
        out.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn slots_and_levels_are_reused() {
        let mut book = FastBook::new();
        for round in 0..100u64 {
            let id = |k: u64| round * 10 + k;
            run(
                &mut book,
                &[
                    &format!("limit {} buy 5 {}", id(1), 100 + round as i64 % 7),
                    &format!("limit {} sell 5 {}", id(2), 200 + round as i64 % 5),
                    &format!("cancel {}", id(1)),
                    &format!("market {} buy 5", id(3)),
                ],
            );
        }
        // Never more than two orders or two levels at once, so the slabs stay tiny.
        assert!(book.orders.items.len() <= 2, "{}", book.orders.items.len());
        assert!(book.levels.items.len() <= 2, "{}", book.levels.items.len());
    }

    #[test]
    fn cancel_from_the_middle_keeps_links_intact() {
        let mut book = FastBook::new();
        run(
            &mut book,
            &[
                "limit 1 buy 1 100",
                "limit 2 buy 2 100",
                "limit 3 buy 3 100",
                "limit 4 buy 4 100",
                "cancel 2",
                "cancel 4",
                "cancel 1",
            ],
        );
        assert_eq!(
            book.depth(Side::Buy, 1),
            [Level {
                price: Price(100),
                qty: Qty(3),
                orders: 1,
            }]
        );
    }

    #[test]
    fn best_cache_follows_new_and_emptied_levels() {
        let mut book = FastBook::new();
        run(
            &mut book,
            &[
                "limit 1 buy 1 100",
                "limit 2 buy 1 102",
                "limit 3 buy 1 101",
                "limit 4 sell 1 105",
                "limit 5 sell 1 103",
                "limit 6 sell 1 104",
            ],
        );
        assert_eq!(book.best_price(Side::Buy), Some(Price(102)));
        assert_eq!(book.best_price(Side::Sell), Some(Price(103)));
        run(&mut book, &["cancel 2", "market 7 buy 1"]);
        assert_eq!(book.best_price(Side::Buy), Some(Price(101)));
        assert_eq!(book.best_price(Side::Sell), Some(Price(104)));
    }

    #[test]
    fn invariant_checker_catches_a_broken_link() {
        let mut book = FastBook::new();
        run(&mut book, &["limit 1 buy 1 100", "limit 2 buy 1 100"]);
        let head = book.levels[book.best[0]].head;
        book.orders[head].next = NIL;
        assert!(book.check_invariants().is_err());
    }
}
