//! The reference book (D8): the simplest structure that is obviously correct.
//!
//! It's the oracle the fast book (M4) is tested against, so clarity beats speed
//! everywhere in this file.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::book::{Level, OrderBook};
use crate::command::{Command, Event, RejectReason};
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
struct Resting {
    id: OrderId,
    qty: Qty,
}

/// Price -> orders at that price, oldest first.
type Levels = BTreeMap<Price, VecDeque<Resting>>;

#[derive(Default)]
pub struct RefBook {
    bids: Levels,
    asks: Levels,
    /// Where each resting order lives, so cancel can find it.
    resting: HashMap<OrderId, (Side, Price)>,
    /// Every id accepted this session; reusing one is rejected (D9).
    used: HashSet<OrderId>,
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

    /// A new limit (`limit = Some`) or market (`limit = None`) order.
    fn submit(
        &mut self,
        id: OrderId,
        side: Side,
        qty: Qty,
        limit: Option<Price>,
        out: &mut Vec<Event>,
    ) {
        let reject = if qty.0 == 0 {
            Some(RejectReason::ZeroQty)
        } else if self.used.contains(&id) {
            Some(RejectReason::DuplicateId)
        } else {
            None
        };
        if let Some(reason) = reject {
            out.push(Event::Rejected { id, reason });
            return;
        }
        self.used.insert(id);
        out.push(Event::Accepted { id });

        let remaining = self.take(id, side, qty, limit, out);
        if remaining.0 == 0 {
            return;
        }
        match limit {
            Some(price) => {
                let levels = match side {
                    Side::Buy => &mut self.bids,
                    Side::Sell => &mut self.asks,
                };
                levels
                    .entry(price)
                    .or_default()
                    .push_back(Resting { id, qty: remaining });
                self.resting.insert(id, (side, price));
            }
            // A market order never rests: whatever the book couldn't fill is cancelled.
            None => out.push(Event::Cancelled { id, remaining }),
        }
    }

    /// Match `qty` against the opposite side: best price first, oldest order first
    /// within a price, for as long as the price crosses `limit` (`None`: any price).
    /// Every fill trades at the resting (maker) order's price. Returns what's left.
    fn take(
        &mut self,
        taker: OrderId,
        side: Side,
        mut qty: Qty,
        limit: Option<Price>,
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
                if maker.qty.0 == 0 {
                    let id = maker.id;
                    queue.pop_front();
                    self.resting.remove(&id);
                }
            }
            if queue.is_empty() {
                level.remove();
            }
        }
        qty
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        // Filled, cancelled and never-seen ids all land here: none of them is resting.
        let Some((side, price)) = self.resting.remove(&id) else {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
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
        out.push(Event::Cancelled {
            id,
            remaining: order.qty,
        });
    }
}

impl OrderBook for RefBook {
    fn apply(&mut self, cmd: &Command, out: &mut Vec<Event>) {
        match *cmd {
            Command::Limit {
                id,
                side,
                qty,
                price,
            } => self.submit(id, side, qty, Some(price), out),
            Command::Market { id, side, qty } => self.submit(id, side, qty, None, out),
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
        if let (Some((bid, _)), Some((ask, _))) =
            (self.bids.last_key_value(), self.asks.first_key_value())
        {
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
                for order in queue {
                    count += 1;
                    if order.qty.0 == 0 {
                        return Err(format!("order {} rests with zero qty", order.id));
                    }
                    if self.resting.get(&order.id) != Some(&(side, price)) {
                        return Err(format!("order {} missing from the index", order.id));
                    }
                    if !self.used.contains(&order.id) {
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
    fn invariant_checker_catches_a_crossed_book() {
        let mut book = RefBook::new();
        // Bypass matching to build a book that apply() could never produce.
        for (id, side, price) in [(1, Side::Buy, 101), (2, Side::Sell, 100)] {
            let id = OrderId(id);
            let levels = match side {
                Side::Buy => &mut book.bids,
                Side::Sell => &mut book.asks,
            };
            levels
                .entry(Price(price))
                .or_default()
                .push_back(Resting { id, qty: Qty(1) });
            book.resting.insert(id, (side, Price(price)));
            book.used.insert(id);
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
