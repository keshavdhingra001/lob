//! The fast book (D19–D22): same behaviour as the reference book, event for event,
//! with cheaper data structures.
//!
//! - Orders live in a slab (`Vec` + free list) and are addressed by `u32` index, so
//!   resting an order reuses a slot instead of allocating once the book is warm.
//! - Each price level is an intrusive doubly linked list threaded through the slab
//!   (`prev`/`next`), with head, tail, total quantity and order count kept on the level.
//! - An id -> slot map makes cancel and modify O(1) to find and O(1) to unlink.
//! - The best level on each side is cached; the price ladder (D33) is only searched when a
//!   level is created or the best level empties.
//! - An iceberg (D83) is an ordinary node holding its shown slice, plus a flag; its peak and
//!   hidden quantity live in a side map, and each level's hidden total in a parallel `Vec`,
//!   so neither struct grows past 32 bytes (D87).
//!
//! Correctness is defined by `RefBook`: the differential tests feed both books the same
//! commands and require identical events.

use std::collections::HashMap;
use std::num::NonZeroU16;

use crate::book::{check_peak, BookConfig, Level, OrderBook};
use crate::command::{Command, Event, NewOrder, RejectReason, Stp, StpAction, TimeInForce};
use crate::hash::IdBuildHasher;
use crate::ladder::Ladder;
use crate::snapshot::{BookState, RestingOrder};
use crate::types::{OrderId, Price, Qty, Side};

/// "No index": the end of a list, or no best level.
const NIL: u32 = u32::MAX;

/// A `Vec` with a free list. Indices stay valid until removed, then get reused.
struct Slab<T> {
    items: Vec<T>,
    free: Vec<u32>,
}

impl<T> Slab<T> {
    fn with_capacity(n: usize) -> Self {
        Slab {
            items: Vec::with_capacity(n),
            free: Vec::with_capacity(n),
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
    /// STP group, 0 for none (D72). `Option<Stp>` would make the node 40 bytes.
    group: u16,
    /// The order's own STP action; meaningless when `group` is 0.
    action: StpAction,
    /// `POST_ONLY` and `ICEBERG` (D87).
    flags: u8,
}

const POST_ONLY: u8 = 1;
/// The order has an entry in `FastBook::icebergs`.
const ICEBERG: u8 = 2;

impl Node {
    fn stp(&self) -> Option<Stp> {
        NonZeroU16::new(self.group).map(|group| Stp {
            group,
            action: self.action,
        })
    }

    fn post_only(&self) -> bool {
        self.flags & POST_ONLY != 0
    }

    fn iceberg(&self) -> bool {
        self.flags & ICEBERG != 0
    }
}

/// What an iceberg keeps off its node (D87).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reserve {
    peak: u64,
    /// Open quantity not shown yet.
    hidden: u64,
}

/// How an order resting `total` open shows it: all of it, or for an iceberg up to its
/// peak with the rest hidden (D83).
fn split(total: u64, peak: Option<Qty>) -> (u64, Option<Reserve>) {
    match peak {
        None => (total, None),
        Some(Qty(peak)) => {
            let shown = total.min(peak);
            let hidden = total - shown;
            (shown, Some(Reserve { peak, hidden }))
        }
    }
}

/// The taker's action if `maker` is in its STP group (D70). A `group` is never 0, so an
/// ungrouped maker never matches.
fn conflict(stp: Option<Stp>, maker: &Node) -> Option<StpAction> {
    stp.filter(|s| s.group.get() == maker.group)
        .map(|s| s.action)
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

// Two orders, or two levels, per 64-byte cache line (D34). A new field that pushed either
// past 32 bytes would halve that, so it has to be a deliberate change here.
const _: () = assert!(std::mem::size_of::<Node>() == 32);
const _: () = assert!(std::mem::size_of::<LevelNode>() == 32);

pub struct FastBook {
    config: BookConfig,
    orders: Slab<Node>,
    levels: Slab<LevelNode>,
    /// Price -> level index, per side: a tick-indexed window plus a tree outside it (D33).
    bids: Ladder,
    asks: Ladder,
    /// Cached best level per side (`NIL` when the side is empty). [bids, asks].
    best: [u32; 2],
    /// Resting order id -> slot, with a one-multiply hash instead of SipHash (D31).
    index: HashMap<OrderId, u32, IdBuildHasher>,
    /// Peak and hidden quantity of every resting iceberg (D87).
    icebergs: HashMap<OrderId, Reserve, IdBuildHasher>,
    /// Hidden quantity per level, indexed like `levels`: a FOK check counts it (D86).
    level_hidden: Vec<u64>,
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

    fn tree(&self, side: Side) -> &Ladder {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn tree_mut(&mut self, side: Side) -> &mut Ladder {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// The best level according to the ladder (ignoring the cache).
    fn tree_best(&self, side: Side) -> u32 {
        let best = match side {
            Side::Buy => self.bids.highest_below(None),
            Side::Sell => self.asks.lowest_above(None),
        };
        best.map_or(NIL, |(_, l)| l)
    }

    /// The best level strictly worse than `price`: the new best once the level at `price`
    /// (the old best) is gone. Searching from there is shorter than from the far end.
    fn next_best(&self, side: Side, price: Price) -> u32 {
        let next = match side {
            Side::Buy => self.bids.highest_below(Some(price)),
            Side::Sell => self.asks.lowest_above(Some(price)),
        };
        next.map_or(NIL, |(_, l)| l)
    }

    /// Visit `side`'s levels best first until `f` returns false. Allocation-free.
    fn visit_best_first(&self, side: Side, f: impl FnMut(Price, u32) -> bool) {
        match side {
            Side::Buy => self.bids.visit_descending(f),
            Side::Sell => self.asks.visit_ascending(f),
        }
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        let l = self.best[side_ix(side)];
        (l != NIL).then(|| self.levels[l].price)
    }

    fn would_cross(&self, side: Side, price: Price) -> bool {
        self.best_price(side.opposite())
            .is_some_and(|best| side.crosses(price, best))
    }

    /// FOK pre-scan: walks levels, not orders, thanks to `total`. Level totals can't see
    /// STP groups, so a grouped taker walks the orders instead (D71).
    fn can_fill(&self, side: Side, qty: Qty, limit: Price, stp: Option<Stp>) -> bool {
        if stp.is_some() {
            return self.can_fill_grouped(side, qty, limit, stp);
        }
        let (mut available, mut enough) = (0, false);
        self.visit_best_first(side.opposite(), |price, l| {
            if !side.crosses(limit, price) {
                return false;
            }
            available += self.levels[l].total + self.level_hidden[l as usize];
            enough = available >= qty.0;
            !enough
        });
        enough
    }

    /// Shown plus hidden quantity of the order in `slot`.
    fn open(&self, slot: u32) -> u64 {
        let node = &self.orders[slot];
        node.qty + self.reserve(node).map_or(0, |r| r.hidden)
    }

    fn reserve(&self, node: &Node) -> Option<Reserve> {
        node.iceberg().then(|| self.icebergs[&node.id])
    }

    /// `can_fill` order by order: same-group orders don't fill. Cancel-oldest skips them;
    /// cancel-newest and cancel-both stop matching at the first one, and of the orders
    /// ahead of it only the shown quantity counts (see `RefBook::can_fill`, D86).
    fn can_fill_grouped(&self, side: Side, qty: Qty, limit: Price, stp: Option<Stp>) -> bool {
        let (mut available, mut enough) = (0, false);
        self.visit_best_first(side.opposite(), |price, l| {
            if !side.crosses(limit, price) {
                return false;
            }
            let (mut shown, mut total, mut slot) = (0, 0, self.levels[l].head);
            while slot != NIL {
                let order = &self.orders[slot];
                match conflict(stp, order) {
                    None => {
                        shown += order.qty;
                        total += self.open(slot);
                    }
                    Some(StpAction::CancelOldest) => {}
                    Some(StpAction::CancelNewest | StpAction::CancelBoth) => {
                        enough = available + shown >= qty.0;
                        return false;
                    }
                }
                slot = order.next;
            }
            available += total;
            enough = available >= qty.0;
            !enough
        });
        enough
    }

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

        let peak = peak.map(|p| Qty(p.get()));
        if tif == TimeInForce::Fok {
            let limit = limit.expect("only limit orders carry a time in force");
            if !self.can_fill(side, qty, limit, stp) {
                out.push(Event::Cancelled { id, remaining: qty });
                return;
            }
        }
        let remaining = self.take(id, side, qty.0, limit, stp, out);
        if remaining == 0 {
            return;
        }
        match (limit, tif) {
            (Some(price), TimeInForce::Gtc | TimeInForce::PostOnly) => {
                let flags = if tif == TimeInForce::PostOnly {
                    POST_ONLY
                } else {
                    0
                };
                let (shown, reserve) = split(remaining, peak);
                let slot = self.rest(id, side, price, shown, flags, stp);
                if let Some(reserve) = reserve {
                    self.hide(slot, reserve);
                }
            }
            _ => out.push(Event::Cancelled {
                id,
                remaining: Qty(remaining),
            }),
        }
    }

    /// Append an order showing `qty` to the back of its price level, creating the level if
    /// needed. Returns its slot, for `hide` if it's an iceberg.
    fn rest(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        qty: u64,
        flags: u8,
        stp: Option<Stp>,
    ) -> u32 {
        let level = match self.tree(side).get(price) {
            Some(l) => l,
            None => self.new_level(side, price),
        };
        let tail = self.levels[level].tail;
        let slot = self.orders.insert(Node {
            id,
            qty,
            level,
            prev: tail,
            next: NIL,
            group: stp.map_or(0, |s| s.group.get()),
            action: stp.map_or(StpAction::CancelNewest, |s| s.action),
            flags,
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
        slot
    }

    /// Make the order resting in `slot` an iceberg with `reserve` (D87). Kept out of `rest`,
    /// so ordinary orders don't carry a reserve through it.
    fn hide(&mut self, slot: u32, reserve: Reserve) {
        let node = &mut self.orders[slot];
        node.flags |= ICEBERG;
        self.level_hidden[node.level as usize] += reserve.hidden;
        self.icebergs.insert(node.id, reserve);
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
        // The slab reuses a freed index or appends one; this grows with it. A reused index
        // already holds 0: a level leaves only once every order (and its hidden part) has.
        if level as usize == self.level_hidden.len() {
            self.level_hidden.push(0);
        }
        // A new level becomes the best if it beats the current best. (Prices in the ladder
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
        self.tree_mut(side).remove(price);
        self.levels.remove(level);
        if self.best[side_ix(side)] == level {
            self.best[side_ix(side)] = self.next_best(side, price);
        }
    }

    /// Take an order out of its level's list and free its slot. Removes the level if it
    /// empties. Returns the node and, for an iceberg, what it kept hidden.
    fn unlink(&mut self, slot: u32) -> (Node, Option<Reserve>) {
        let node = self.orders[slot];
        let mut reserve = None;
        if node.iceberg() {
            let r = self
                .icebergs
                .remove(&node.id)
                .expect("an iceberg has a reserve");
            self.level_hidden[node.level as usize] -= r.hidden;
            reserve = Some(r);
        }
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
        (node, reserve)
    }

    /// Show an iceberg's next slice once its shown quantity has traded away: at the back of
    /// its level (D84), where the current sweep may reach it again (D85). Returns false if
    /// it has nothing hidden left, so it's done.
    fn replenish(&mut self, slot: u32, out: &mut Vec<Event>) -> bool {
        let Node { id, level, .. } = self.orders[slot];
        let reserve = self
            .icebergs
            .get_mut(&id)
            .expect("an iceberg has a reserve");
        if reserve.hidden == 0 {
            return false;
        }
        let qty = reserve.peak.min(reserve.hidden);
        reserve.hidden -= qty;
        self.level_hidden[level as usize] -= qty;
        self.levels[level].total += qty;
        self.orders[slot].qty = qty;
        out.push(Event::Replenished { id, qty: Qty(qty) });
        let Node { prev, next, .. } = self.orders[slot];
        if next == NIL {
            return true; // Already last.
        }
        // Detach (it isn't the tail, so `next` exists), then append.
        match prev {
            NIL => self.levels[level].head = next,
            prev => self.orders[prev].next = next,
        }
        self.orders[next].prev = prev;
        let tail = self.levels[level].tail;
        self.orders[tail].next = slot;
        self.orders[slot].prev = tail;
        self.orders[slot].next = NIL;
        self.levels[level].tail = slot;
        true
    }

    /// Match against the opposite side; see `RefBook::take`. Returns the unfilled quantity,
    /// zero if self-trade prevention cancelled the taker.
    fn take(
        &mut self,
        taker: OrderId,
        side: Side,
        mut qty: u64,
        limit: Option<Price>,
        stp: Option<Stp>,
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
                if let Some(action) = conflict(stp, maker) {
                    if action != StpAction::CancelNewest {
                        // The whole iceberg goes, hidden quantity too (D86).
                        let remaining = Qty(self.open(slot));
                        let (node, _) = self.unlink(slot);
                        self.index.remove(&node.id);
                        out.push(Event::SelfTradeCancelled {
                            id: node.id,
                            remaining,
                        });
                    }
                    if action != StpAction::CancelOldest {
                        out.push(Event::SelfTradeCancelled {
                            id: taker,
                            remaining: Qty(qty),
                        });
                        qty = 0;
                    }
                    continue;
                }
                let fill = qty.min(maker.qty);
                maker.qty -= fill;
                let (maker_id, maker_done, iceberg) = (maker.id, maker.qty == 0, maker.iceberg());
                out.push(Event::Trade {
                    taker,
                    maker: maker_id,
                    taker_side: side,
                    qty: Qty(fill),
                    price,
                });
                qty -= fill;
                self.levels[level].total -= fill;
                if maker_done && !(iceberg && self.replenish(slot, out)) {
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
                let (node, reserve) = self.unlink(slot);
                out.push(Event::Cancelled {
                    id,
                    remaining: Qty(node.qty + reserve.map_or(0, |r| r.hidden)),
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
        if let Err(reason) = self.config.check(qty, Some(price)) {
            return reject(out, reason);
        }
        let node = self.orders[slot];
        let lv = self.levels[node.level];
        if price == lv.price && qty.0 <= self.open(slot) {
            // An iceberg loses hidden quantity first, then shown (D88).
            let mut cut = self.open(slot) - qty.0;
            if let Some(reserve) = self.icebergs.get_mut(&id) {
                let from_hidden = cut.min(reserve.hidden);
                reserve.hidden -= from_hidden;
                self.level_hidden[node.level as usize] -= from_hidden;
                cut -= from_hidden;
            }
            self.levels[node.level].total -= cut;
            self.orders[slot].qty -= cut;
            out.push(Event::Modified { id, qty, price });
            return;
        }
        if node.post_only() && self.would_cross(lv.side, price) {
            return reject(out, RejectReason::WouldCross);
        }
        self.index.remove(&id);
        let (_, reserve) = self.unlink(slot);
        out.push(Event::Modified { id, qty, price });
        let stp = node.stp();
        let remaining = self.take(id, lv.side, qty.0, Some(price), stp, out);
        if remaining > 0 {
            let (shown, reserve) = split(remaining, reserve.map(|r| Qty(r.peak)));
            let slot = self.rest(id, lv.side, price, shown, node.flags & POST_ONLY, stp);
            if let Some(reserve) = reserve {
                self.hide(slot, reserve);
            }
        }
    }
}

impl FastBook {
    /// A book with room for `orders` resting orders reserved up front: the order and level
    /// slabs, their free lists and the id index (D32). Below that, applying a command never
    /// allocates once both sides' ladder windows exist (their first order allocates them).
    /// Exchanges size their pools at startup for the same reason.
    pub fn with_capacity(config: BookConfig, orders: usize) -> Self {
        config.validate();
        FastBook {
            config,
            orders: Slab::with_capacity(orders),
            levels: Slab::with_capacity(orders),
            bids: Ladder::new(config.tick_size),
            asks: Ladder::new(config.tick_size),
            best: [NIL, NIL],
            index: HashMap::with_capacity_and_hasher(orders, Default::default()),
            icebergs: HashMap::with_capacity_and_hasher(orders, Default::default()),
            level_hidden: Vec::with_capacity(orders),
            last_id: None,
        }
    }

    /// Levels outside the ladder windows (D33), both sides. Those take the slower,
    /// allocating tree path, so benchmarks and tests report it.
    pub fn overflow_levels(&self) -> usize {
        self.bids.overflow_len() + self.asks.overflow_len()
    }
}

impl Default for FastBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook for FastBook {
    fn with_config(config: BookConfig) -> Self {
        FastBook::with_capacity(config, 0)
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
        let mut out = Vec::new();
        if n == 0 {
            return out;
        }
        self.visit_best_first(side, |_, l| {
            let lv = &self.levels[l];
            out.push(Level {
                price: lv.price,
                qty: Qty(lv.total),
                orders: lv.count as usize,
            });
            out.len() < n
        });
        out
    }

    fn check_invariants(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best_price(Side::Buy), self.best_price(Side::Sell)) {
            if bid >= ask {
                return Err(format!("crossed book: best bid {bid} >= best ask {ask}"));
            }
        }
        let (mut orders, mut icebergs) = (0, 0);
        for side in [Side::Buy, Side::Sell] {
            if self.best[side_ix(side)] != self.tree_best(side) {
                return Err(format!("stale best-level cache on the {side} side"));
            }
            self.tree(side).check()?;
            let mut levels = Vec::new();
            self.visit_best_first(side, |price, l| {
                levels.push((price, l));
                true
            });
            for (price, l) in levels {
                let lv = &self.levels[l];
                if lv.price != price || lv.side != side {
                    return Err(format!("level {l} filed under {side} {price} is {lv:?}"));
                }
                if price.0 % self.config.tick_size != 0 {
                    return Err(format!("{side} level at {price} is off the tick grid"));
                }
                // Walk the list forwards, checking back links, ownership and the totals.
                let (mut total, mut count, mut prev, mut slot) = (0, 0, NIL, lv.head);
                let mut hidden = 0;
                while slot != NIL {
                    let node = &self.orders[slot];
                    if node.prev != prev || node.level != l {
                        return Err(format!("broken links at slot {slot} in {side} {price}"));
                    }
                    if node.qty == 0 {
                        return Err(format!("order {} rests with zero qty", node.id));
                    }
                    let reserve = self.icebergs.get(&node.id);
                    if node.iceberg() != reserve.is_some()
                        || reserve.is_some_and(|r| node.qty > r.peak)
                    {
                        return Err(format!("order {} has a bad reserve", node.id));
                    }
                    hidden += reserve.map_or(0, |r| r.hidden);
                    icebergs += reserve.is_some() as usize;
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
                if hidden != self.level_hidden[l as usize] {
                    return Err(format!("{side} {price}: hidden total is off"));
                }
                orders += count;
            }
        }
        if icebergs != self.icebergs.len() {
            return Err("iceberg map holds orders that don't rest".to_string());
        }
        if orders != self.index.len() || orders != self.orders.live() {
            return Err(format!(
                "lists hold {orders} orders, index {}, slab {}",
                self.index.len(),
                self.orders.live()
            ));
        }
        if self.levels.live() != self.bids.len() + self.asks.len() {
            return Err("level slab and price ladders disagree".to_string());
        }
        Ok(())
    }

    fn state(&self) -> BookState {
        let mut orders = Vec::with_capacity(self.index.len());
        for side in [Side::Buy, Side::Sell] {
            self.visit_best_first(side, |price, l| {
                let mut slot = self.levels[l].head;
                while slot != NIL {
                    let node = &self.orders[slot];
                    let reserve = self.reserve(node);
                    orders.push(RestingOrder {
                        id: node.id,
                        side,
                        price,
                        qty: Qty(node.qty),
                        peak: reserve.map(|r| Qty(r.peak)),
                        hidden: Qty(reserve.map_or(0, |r| r.hidden)),
                        post_only: node.post_only(),
                        stp: node.stp(),
                    });
                    slot = node.next;
                }
                true
            });
        }
        BookState {
            config: self.config,
            last_id: self.last_id,
            orders,
        }
    }

    fn from_state(state: &BookState) -> Result<Self, &'static str> {
        state.validate()?;
        let mut book = FastBook::with_capacity(state.config, state.orders.len());
        book.last_id = state.last_id;
        // Priority order, so each order joins the tail of its level as it once did.
        for o in &state.orders {
            let flags = if o.post_only { POST_ONLY } else { 0 };
            let reserve = o.peak.map(|peak| Reserve {
                peak: peak.0,
                hidden: o.hidden.0,
            });
            let slot = book.rest(o.id, o.side, o.price, o.qty.0, flags, o.stp);
            if let Some(reserve) = reserve {
                book.hide(slot, reserve);
            }
        }
        Ok(book)
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
    fn extreme_prices_match_the_reference_book() {
        // The ladder's window offset once overflowed `i64` here (found by M14's snapshot
        // property): a window centred near 0, then a price near `i64::MIN`.
        let lines = [
            "limit 1 buy 5 100",
            "limit 2 buy 5 -9223372036854775808",
            "limit 3 sell 5 9223372036854775807",
            "limit 4 sell 2 -9223372036854775808",
            "modify 2 3 9223372036854775806",
            "market 5 sell 20",
            "limit 6 sell 1 -9223372036854775807",
            "market 7 buy 20",
        ];
        let mut reference = crate::RefBook::new();
        let mut want = Vec::new();
        for line in lines {
            reference.apply(&line.parse().unwrap(), &mut want);
        }
        let want: Vec<String> = want.iter().map(|e| e.to_string()).collect();
        assert_eq!(run(&mut FastBook::new(), &lines), want);
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
