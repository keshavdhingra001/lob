//! Quantity conservation, checked from the outside (D13).
//!
//! The ledger never looks inside a book. It reads the commands and the events, keeps
//! its own count of every live order's open quantity, and after each command checks
//! that count against the book's public depth. If a book loses, invents or double-fills
//! a single unit, the totals stop matching.
//!
//! It also checks self-trade prevention from the outside (D70): no trade between two
//! orders of one group, STP cancels only between orders of one group and only as the
//! taker's action allows, and nothing after the taker's own STP cancel. And it checks
//! that a FOK order fills completely or not at all.
//!
//! Icebergs (D83-D88): the ledger keeps each order's shown part apart from its open
//! total. A trade may only take shown quantity, a slice is replenished only once the
//! shown part is gone and is exactly `min(peak, hidden)`, and the book's depth must equal
//! the shown parts while cancels report the totals.

use std::collections::HashMap;
use std::num::NonZeroU64;

use crate::book::OrderBook;
use crate::command::{Command, Event, Stp, StpAction, TimeInForce};
use crate::types::{OrderId, Price, Qty, Side};

/// One live order as the ledger sees it.
#[derive(Clone, Copy, Debug)]
struct Live {
    /// Open (unfilled, uncancelled) quantity, shown and hidden.
    open: u64,
    /// The part the book shows. Set when the order rests.
    shown: u64,
    peak: Option<u64>,
    /// Its limit; `None` for a market order.
    price: Option<Price>,
}

#[derive(Default)]
pub struct Ledger {
    live: HashMap<OrderId, Live>,
    /// The STP group and action of every live order that has one.
    stp: HashMap<OrderId, Stp>,
    /// The order the current command is working (a new one, or one a modify re-entered):
    /// what's left of it rests at the end of the command.
    taker: Option<OrderId>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account for one command's events, then compare against `book`.
    pub fn observe<B: OrderBook>(
        &mut self,
        cmd: &Command,
        events: &[Event],
        book: &B,
    ) -> Result<(), String> {
        let ctx = |msg: String| format!("after `{cmd}`: {msg}");
        // The price a taker may not trade beyond, and the side it trades on.
        let (limit, side): (Option<Price>, Option<Side>) = match *cmd {
            Command::Limit { side, price, .. } => (Some(price), Some(side)),
            Command::Market { side, .. } => (None, Some(side)),
            Command::Modify { price, .. } => (Some(price), None),
            Command::Cancel { .. } => (None, None),
        };
        let taker_stopped = events
            .iter()
            .position(|e| matches!(*e, Event::SelfTradeCancelled { id, .. } if id == cmd.id()));
        if taker_stopped.is_some_and(|i| i + 1 != events.len()) {
            return Err(ctx("events after the taker's STP cancel".into()));
        }
        self.taker = None;
        for event in events {
            self.apply_event(cmd, event, limit, side).map_err(ctx)?;
        }
        if let Some(live) = self.taker.and_then(|id| self.live.get_mut(&id)) {
            // What's left rests, an iceberg showing up to its peak.
            live.shown = live.peak.map_or(live.open, |peak| peak.min(live.open));
        }
        self.live.retain(|_, l| l.open > 0);
        let live = &self.live;
        self.stp.retain(|id, _| live.contains_key(id));

        // Market, IOC and FOK orders must be done by the end of their own command. (Only
        // if accepted: a rejected id that isn't increasing may belong to an older order that rests.)
        let accepted = events.contains(&Event::Accepted { id: cmd.id() });
        let never_rests = accepted
            && matches!(
                cmd,
                Command::Market { .. }
                    | Command::Limit {
                        tif: TimeInForce::Ioc | TimeInForce::Fok,
                        ..
                    }
            );
        if never_rests && self.live.contains_key(&cmd.id()) {
            return Err(ctx(format!("order {} is still open", cmd.id())));
        }
        if let (
            true,
            Command::Limit {
                id,
                qty,
                tif: TimeInForce::Fok,
                ..
            },
        ) = (accepted, cmd)
        {
            let filled: u64 = events
                .iter()
                .map(|e| match *e {
                    Event::Trade { taker, qty, .. } if taker == *id => qty.0,
                    _ => 0,
                })
                .sum();
            if filled != 0 && filled != qty.0 {
                return Err(ctx(format!("FOK order {id} filled {filled} of {qty}")));
            }
        }
        if let Some((id, l)) = self.live.iter().find(|(_, l)| {
            l.shown == 0 || l.shown > l.open || l.peak.map_or(l.shown != l.open, |p| l.shown > p)
        }) {
            return Err(ctx(format!(
                "order {id} shows {} of {} open",
                l.shown, l.open
            )));
        }

        let mut book_qty = 0;
        let mut book_orders = 0;
        for s in [Side::Buy, Side::Sell] {
            for level in book.depth(s, usize::MAX) {
                book_qty += level.qty.0;
                book_orders += level.orders;
            }
        }
        let shown: u64 = self.live.values().map(|l| l.shown).sum();
        if (shown, self.live.len()) != (book_qty, book_orders) {
            return Err(ctx(format!(
                "ledger has {shown} shown in {} orders, book shows {book_qty} in {book_orders}",
                self.live.len()
            )));
        }
        Ok(())
    }

    fn apply_event(
        &mut self,
        cmd: &Command,
        event: &Event,
        limit: Option<Price>,
        side: Option<Side>,
    ) -> Result<(), String> {
        match *event {
            Event::Accepted { id } => {
                let order = cmd
                    .new_order()
                    .filter(|o| o.id == id)
                    .ok_or_else(|| format!("`accepted {id}` doesn't match the command"))?;
                let live = Live {
                    open: order.qty.0,
                    shown: 0,
                    peak: order.peak.map(NonZeroU64::get),
                    price: order.limit,
                };
                if self.live.insert(id, live).is_some() {
                    return Err(format!("order {id} accepted while already open"));
                }
                self.taker = Some(id);
                if let Some(stp) = order.stp {
                    self.stp.insert(id, stp);
                }
            }
            Event::Rejected { id, .. } => {
                if id != cmd.id() {
                    return Err(format!("`rejected {id}` doesn't match the command"));
                }
            }
            Event::Modified { id, qty, price } => {
                let live = self
                    .live
                    .get_mut(&id)
                    .ok_or_else(|| format!("modified {id}, which isn't open"))?;
                if live.price == Some(price) && qty.0 <= live.open {
                    // In place (D11): hidden quantity goes first (D88).
                    let cut = live.open - qty.0;
                    let hidden = live.open - live.shown;
                    live.shown -= cut - cut.min(hidden);
                } else {
                    live.price = Some(price);
                    self.taker = Some(id);
                }
                live.open = qty.0;
            }
            Event::Trade {
                taker,
                maker,
                taker_side,
                qty,
                price,
            } => {
                if taker != cmd.id() || taker == maker || qty.0 == 0 {
                    return Err(format!("bad trade {event}"));
                }
                if side.is_some_and(|s| s != taker_side) {
                    return Err(format!("trade on the wrong side: {event}"));
                }
                if limit.is_some_and(|limit| !taker_side.crosses(limit, price)) {
                    return Err(format!("trade through the taker's limit: {event}"));
                }
                if self.conflict(taker, maker).is_some() {
                    return Err(format!("self-trade: {event}"));
                }
                for id in [taker, maker] {
                    let live = self
                        .live
                        .get_mut(&id)
                        .ok_or_else(|| format!("{event}: order {id} isn't open"))?;
                    live.open = live.open.checked_sub(qty.0).ok_or_else(|| {
                        format!("{event}: order {id} only had {} open", live.open)
                    })?;
                }
                // A resting order trades only what it shows (D85).
                let live = self.live.get_mut(&maker).expect("checked above");
                live.shown = live
                    .shown
                    .checked_sub(qty.0)
                    .ok_or_else(|| format!("{event}: order {maker} only showed {}", live.shown))?;
            }
            Event::Replenished { id, qty } => {
                let live = self
                    .live
                    .get_mut(&id)
                    .filter(|_| self.taker != Some(id))
                    .ok_or_else(|| format!("{event}: not a resting order"))?;
                let want = live.peak.map_or(0, |peak| peak.min(live.open));
                if live.shown != 0 || qty.0 == 0 || qty.0 != want {
                    return Err(format!(
                        "{event}: showed {} of {} open, peak {:?}",
                        live.shown, live.open, live.peak
                    ));
                }
                live.shown = qty.0;
            }
            Event::SelfTradeCancelled { id, remaining } => {
                // The taker's action decides who may be cancelled (D68, D69).
                let taker = cmd.id();
                let action = self
                    .conflict(taker, id)
                    .ok_or_else(|| format!("{event}: not in the taker's STP group"))?;
                let allowed = if id == taker {
                    action != StpAction::CancelOldest
                } else {
                    action != StpAction::CancelNewest
                };
                if !allowed {
                    return Err(format!("{event}: the taker's action is {action}"));
                }
                self.close(id, remaining)?;
            }
            Event::Cancelled { id, remaining } => self.close(id, remaining)?,
        }
        Ok(())
    }

    /// The taker's STP action if `taker` and `maker` are live in one group.
    fn conflict(&self, taker: OrderId, maker: OrderId) -> Option<StpAction> {
        Stp::conflict(self.stp.get(&taker).copied(), self.stp.get(&maker).copied())
    }

    /// An order is done with `remaining` unfilled, which must be all it had open.
    fn close(&mut self, id: OrderId, remaining: Qty) -> Result<(), String> {
        let open = self.live.remove(&id).map_or(0, |l| l.open);
        if open != remaining.0 || open == 0 {
            return Err(format!(
                "cancelled {id} with {remaining} but the ledger had {open} open"
            ));
        }
        Ok(())
    }

    /// Total open quantity across live orders, shown and hidden.
    pub fn open_qty(&self) -> Qty {
        Qty(self.live.values().map(|l| l.open).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::RefBook;

    fn run(lines: &[&str]) -> Result<(), String> {
        let mut book = RefBook::new();
        let mut ledger = Ledger::new();
        for line in lines {
            let cmd: Command = line.parse().unwrap();
            let mut events = Vec::new();
            book.apply(&cmd, &mut events);
            ledger.observe(&cmd, &events, &book)?;
        }
        Ok(())
    }

    #[test]
    fn a_correct_session_balances() {
        run(&[
            "limit 1 sell 10 100",
            "limit 2 sell 10 101",
            "limit 3 buy 15 101",
            "modify 2 2 101",
            "market 4 buy 1",
            "limit 5 buy 7 99 ioc",
            "cancel 2",
        ])
        .unwrap();
    }

    #[test]
    fn catches_a_lying_book() {
        let mut book = RefBook::new();
        let mut ledger = Ledger::new();
        let cmd: Command = "limit 1 sell 10 100".parse().unwrap();
        let mut events = Vec::new();
        book.apply(&cmd, &mut events);
        // Drop the accept: the book now holds quantity the ledger never saw.
        assert!(ledger.observe(&cmd, &[], &book).is_err());

        let mut ledger = Ledger::new();
        ledger.observe(&cmd, &events, &book).unwrap();
        let cmd: Command = "market 2 buy 4".parse().unwrap();
        let mut events = Vec::new();
        book.apply(&cmd, &mut events);
        // Overstate the fill.
        let forged: Vec<Event> = events
            .iter()
            .map(|e| match *e {
                Event::Trade {
                    taker,
                    maker,
                    taker_side,
                    price,
                    ..
                } => Event::Trade {
                    taker,
                    maker,
                    taker_side,
                    qty: Qty(5),
                    price,
                },
                other => other,
            })
            .collect();
        assert!(ledger.observe(&cmd, &forged, &book).is_err());
    }

    /// Run `setup`, then check `forge(events of last)` against the ledger.
    fn forged(setup: &[&str], last: &str, forge: impl Fn(&mut Vec<Event>)) -> Result<(), String> {
        let mut book = RefBook::new();
        let mut ledger = Ledger::new();
        let mut events = Vec::new();
        for line in setup {
            let cmd: Command = line.parse().unwrap();
            events.clear();
            book.apply(&cmd, &mut events);
            ledger.observe(&cmd, &events, &book).unwrap();
        }
        let cmd: Command = last.parse().unwrap();
        events.clear();
        book.apply(&cmd, &mut events);
        forge(&mut events);
        ledger.observe(&cmd, &events, &book)
    }

    #[test]
    fn catches_self_trade_prevention_errors() {
        let setup = ["limit 1 sell 5 100 g=1 stp=cn", "limit 2 sell 5 100"];
        let stp = |id, qty| Event::SelfTradeCancelled {
            id: OrderId(id),
            remaining: Qty(qty),
        };
        let trade = |maker| Event::Trade {
            taker: OrderId(3),
            maker: OrderId(maker),
            taker_side: Side::Buy,
            qty: Qty(5),
            price: Price(100),
        };
        // The honest events pass.
        forged(&setup, "limit 3 buy 10 100 g=1 stp=co", |_| {}).unwrap();
        // A self-trade printed instead of the cancel.
        let err = forged(&setup, "limit 3 buy 10 100 g=1 stp=co", |e| e[1] = trade(1));
        assert!(err.unwrap_err().contains("self-trade"));
        // Cancel-newest must not cancel the resting order...
        let err = forged(&setup, "limit 3 buy 10 100 g=1 stp=cn", |e| {
            *e = vec![e[0], stp(1, 5)]
        });
        assert!(err.unwrap_err().contains("action is cn"));
        // ...and cancel-oldest must not cancel the taker.
        let err = forged(&setup, "limit 3 buy 10 100 g=1 stp=co", |e| {
            *e = vec![e[0], stp(3, 10)]
        });
        assert!(err.unwrap_err().contains("action is co"));
        // No STP cancel across groups, or for an ungrouped order.
        let err = forged(&setup, "limit 3 buy 10 100 g=2 stp=co", |e| {
            *e = vec![e[0], stp(1, 5), trade(2)]
        });
        assert!(err.unwrap_err().contains("not in the taker's STP group"));
        let err = forged(&setup, "limit 3 buy 10 100", |e| {
            *e = vec![e[0], stp(3, 10)]
        });
        assert!(err.unwrap_err().contains("not in the taker's STP group"));
        // Nothing may follow the taker's own STP cancel.
        let err = forged(&setup, "limit 3 buy 10 100 g=1 stp=cn", |e| {
            e.push(trade(2))
        });
        assert!(err.unwrap_err().contains("after the taker's STP cancel"));
    }

    #[test]
    fn catches_iceberg_errors() {
        let setup = ["limit 1 sell 10 100 peak=4", "limit 2 sell 3 100"];
        let last = "limit 3 buy 6 100";
        let trade = |maker, qty| Event::Trade {
            taker: OrderId(3),
            maker: OrderId(maker),
            taker_side: Side::Buy,
            qty: Qty(qty),
            price: Price(100),
        };
        let replenished = |qty| Event::Replenished {
            id: OrderId(1),
            qty: Qty(qty),
        };
        // Honest: 4 from the iceberg, its next slice of 4 goes to the back, 2 from order 2.
        forged(&setup, last, |e| {
            assert_eq!(*e, [e[0], trade(1, 4), replenished(4), trade(2, 2)])
        })
        .unwrap();
        // Trading hidden quantity directly.
        let err = forged(&setup, last, |e| *e = vec![e[0], trade(1, 6)]);
        assert!(err.unwrap_err().contains("only showed 4"));
        // A slice that isn't min(peak, hidden), or one while some still shows.
        let err = forged(&setup, last, |e| e[2] = replenished(5));
        assert!(err.unwrap_err().contains("replenished 1 5"));
        let err = forged(&setup, last, |e| {
            *e = vec![e[0], trade(1, 2), replenished(4), trade(1, 4)]
        });
        assert!(err.unwrap_err().contains("showed 2 of 8"));
        // No replenish: the iceberg shows nothing but still has 6 open.
        let err = forged(&setup, last, |e| *e = vec![e[0], trade(1, 4), trade(2, 2)]);
        assert!(err.unwrap_err().contains("shows 0 of 6"));
        // A cancel must report the hidden quantity too.
        let cancel = |qty| Event::Cancelled {
            id: OrderId(1),
            remaining: Qty(qty),
        };
        forged(&setup, "cancel 1", |e| assert_eq!(*e, [cancel(10)])).unwrap();
        assert!(forged(&setup, "cancel 1", |e| *e = vec![cancel(4)]).is_err());
    }

    #[test]
    fn catches_a_partly_filled_fok() {
        let setup = ["limit 1 sell 5 100", "limit 2 sell 5 100 g=1 stp=co"];
        let last = "limit 3 buy 10 100 fok g=1 stp=cn";
        forged(&setup, last, |_| {}).unwrap();
        // What a wrong pre-check would let through: a fill, then the STP cancel.
        let err = forged(&setup, last, |e| {
            *e = vec![
                e[0],
                Event::Trade {
                    taker: OrderId(3),
                    maker: OrderId(1),
                    taker_side: Side::Buy,
                    qty: Qty(5),
                    price: Price(100),
                },
                Event::SelfTradeCancelled {
                    id: OrderId(3),
                    remaining: Qty(5),
                },
            ]
        });
        assert!(err.unwrap_err().contains("filled 5 of 10"));
    }
}
