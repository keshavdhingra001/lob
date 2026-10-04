//! Quantity conservation, checked from the outside (D13).
//!
//! The ledger never looks inside a book. It reads the commands and the events, keeps
//! its own count of every live order's open quantity, and after each command checks
//! that count against the book's public depth. If a book loses, invents or double-fills
//! a single unit, the totals stop matching.

use std::collections::HashMap;

use crate::book::OrderBook;
use crate::command::{Command, Event, TimeInForce};
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Default)]
pub struct Ledger {
    /// Open (unfilled, uncancelled) quantity of every live order.
    open: HashMap<OrderId, u64>,
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
        for event in events {
            self.apply_event(cmd, event, limit, side).map_err(ctx)?;
        }
        self.open.retain(|_, qty| *qty > 0);

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
        if never_rests && self.open.contains_key(&cmd.id()) {
            return Err(ctx(format!("order {} is still open", cmd.id())));
        }

        let mut book_qty = 0;
        let mut book_orders = 0;
        for s in [Side::Buy, Side::Sell] {
            for level in book.depth(s, usize::MAX) {
                book_qty += level.qty.0;
                book_orders += level.orders;
            }
        }
        let ledger_qty: u64 = self.open.values().sum();
        if (ledger_qty, self.open.len()) != (book_qty, book_orders) {
            return Err(ctx(format!(
                "ledger has {ledger_qty} open in {} orders, book shows {book_qty} in {book_orders}",
                self.open.len()
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
                let qty = match *cmd {
                    Command::Limit { id: c, qty, .. } | Command::Market { id: c, qty, .. }
                        if c == id =>
                    {
                        qty
                    }
                    _ => return Err(format!("`accepted {id}` doesn't match the command")),
                };
                if self.open.insert(id, qty.0).is_some() {
                    return Err(format!("order {id} accepted while already open"));
                }
            }
            Event::Rejected { id, .. } => {
                if id != cmd.id() {
                    return Err(format!("`rejected {id}` doesn't match the command"));
                }
            }
            Event::Modified { id, qty, .. } => {
                let open = self
                    .open
                    .get_mut(&id)
                    .ok_or(format!("modified {id}, which isn't open"))?;
                *open = qty.0;
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
                for id in [taker, maker] {
                    let open = self
                        .open
                        .get_mut(&id)
                        .ok_or(format!("{event}: order {id} isn't open"))?;
                    *open = open
                        .checked_sub(qty.0)
                        .ok_or(format!("{event}: order {id} only had {open} open"))?;
                }
            }
            Event::Cancelled { id, remaining } => {
                let open = self.open.remove(&id).unwrap_or(0);
                if open != remaining.0 || open == 0 {
                    return Err(format!(
                        "cancelled {id} with {remaining} but the ledger had {open} open"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Total open quantity across live orders.
    pub fn open_qty(&self) -> Qty {
        Qty(self.open.values().sum())
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
}
