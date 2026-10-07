//! One symbol's ITCH order flow as engine commands (D54), so both books can be measured
//! on real flow.
//!
//! ITCH reports trades NASDAQ already made (D37). Here an execution becomes an IOC from
//! the other side at the named order's price, and our engine decides which resting order
//! it fills. That's usually the named one. When it isn't, our book drifts from NASDAQ's,
//! so the translator runs a reference book itself and tracks every order's open size
//! there. It skips cancels for orders our book no longer has, and cancels orders NASDAQ
//! has finished that are still live in ours. The engine is deterministic (D4), so
//! replaying the commands reproduces exactly the book the translator saw.

use std::collections::HashMap;

use crate::hash::IdBuildHasher;
use crate::itch::{Body, Message, Stock};
use crate::{Command, Event, OrderBook, OrderId, Price, Qty, RefBook, Side, TimeInForce};

/// What happened to the symbol's messages. Shares are ITCH shares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowStats {
    /// The symbol's messages after its directory entry, of every type.
    pub messages: u64,
    pub commands: u64,
    /// `A`/`F` adds and the new half of `U` replaces.
    pub adds: u64,
    /// Adds priced off the cent grid, skipped with every later message about them.
    pub sub_penny: u64,
    /// Adds that traded on arrival in our book. NASDAQ's displayed book doesn't cross
    /// outside auctions (D38), so these come from drift or auction leftovers.
    pub adds_traded: u64,
    /// `E` executions, each sent as one IOC.
    pub executions: u64,
    /// IOC shares that filled the order NASDAQ named, another order, or nothing.
    pub named_shares: u64,
    pub other_shares: u64,
    pub unfilled_shares: u64,
    /// `C` executions in crosses, applied as size reductions (we have no auction).
    pub cross_executions: u64,
    /// Cancels sent for orders NASDAQ finished that were still live in our book.
    pub resyncs: u64,
    /// Cancels, partial cancels and cross executions skipped because our book no longer had the order.
    pub gone: u64,
    /// Messages about orders that were never sent (sub-penny).
    pub untracked: u64,
    /// Commands our book rejected (sizes over `max_qty`).
    pub rejects: u64,
}

/// A NASDAQ order we sent, with its size as NASDAQ sees it.
struct Live {
    id: OrderId,
    side: Side,
    price: Price,
    shares: u64,
}

pub struct Translator {
    stock: Stock,
    locate: Option<u16>,
    book: RefBook,
    /// NASDAQ's live orders that we sent, by order reference.
    live: HashMap<u64, Live, IdBuildHasher>,
    /// Open size of each order resting in our book.
    open: HashMap<OrderId, u64, IdBuildHasher>,
    next_id: u64,
    events: Vec<Event>,
    stats: FlowStats,
}

impl Translator {
    pub fn new(stock: Stock) -> Self {
        Translator {
            stock,
            locate: None,
            book: RefBook::new(),
            live: HashMap::default(),
            open: HashMap::default(),
            next_id: 1,
            events: Vec::with_capacity(64),
            stats: FlowStats::default(),
        }
    }

    pub fn stats(&self) -> &FlowStats {
        &self.stats
    }

    /// The book after every command so far: what replaying them produces.
    pub fn book(&self) -> &RefBook {
        &self.book
    }

    /// Append the commands for one message to `out`. Messages for other symbols add nothing.
    pub fn on_message(&mut self, msg: &Message, out: &mut Vec<Command>) {
        if let Body::StockDirectory { stock } = msg.body {
            if stock == self.stock {
                self.locate = Some(msg.header.locate);
            }
            return;
        }
        if self.locate != Some(msg.header.locate) {
            return;
        }
        self.stats.messages += 1;
        match msg.body {
            Body::AddOrder {
                order_ref,
                side,
                shares,
                price,
                ..
            } => self.add(order_ref, side, shares, price, out),
            Body::Executed {
                order_ref, shares, ..
            } => self.execute(order_ref, shares, out),
            Body::ExecutedWithPrice {
                order_ref, shares, ..
            } => {
                self.stats.cross_executions += 1;
                self.reduce(order_ref, shares, out);
            }
            Body::Cancel { order_ref, shares } => self.reduce(order_ref, shares, out),
            Body::Delete { order_ref } => self.delete(order_ref, out),
            Body::Replace {
                old_ref,
                new_ref,
                shares,
                price,
            } => {
                let Some(side) = self.live.get(&old_ref).map(|l| l.side) else {
                    // The old order was never sent, so the replacement is untracked too.
                    self.stats.untracked += 1;
                    return;
                };
                self.delete(old_ref, out);
                self.add(new_ref, side, shares, price, out);
            }
            _ => {}
        }
    }

    fn add(&mut self, order_ref: u64, side: Side, shares: u32, price: u32, out: &mut Vec<Command>) {
        self.stats.adds += 1;
        if !price.is_multiple_of(100) {
            self.stats.sub_penny += 1;
            return;
        }
        let id = self.new_id();
        let price = Price(i64::from(price / 100));
        let shares = u64::from(shares);
        self.live.insert(
            order_ref,
            Live {
                id,
                side,
                price,
                shares,
            },
        );
        self.open.insert(id, shares);
        self.send(
            Command::Limit {
                id,
                side,
                qty: Qty(shares),
                price,
                tif: TimeInForce::Gtc,
            },
            out,
        );
        if self.events.iter().any(|e| matches!(e, Event::Trade { .. })) {
            self.stats.adds_traded += 1;
        }
    }

    fn execute(&mut self, order_ref: u64, shares: u32, out: &mut Vec<Command>) {
        let Some(live) = self.live.get_mut(&order_ref) else {
            self.stats.untracked += 1;
            return;
        };
        self.stats.executions += 1;
        let shares = u64::from(shares);
        live.shares = live.shares.saturating_sub(shares);
        let (named, side, price, done) = (live.id, live.side, live.price, live.shares == 0);
        // The trade happened in the market whether or not our book still has the named
        // order, so the IOC is always sent: it keeps the level's volume in step.
        let taker = self.new_id();
        self.send(
            Command::Limit {
                id: taker,
                side: side.opposite(),
                qty: Qty(shares),
                price,
                tif: TimeInForce::Ioc,
            },
            out,
        );
        for e in &self.events {
            match *e {
                Event::Trade { maker, qty, .. } if maker == named => {
                    self.stats.named_shares += qty.0
                }
                Event::Trade { qty, .. } => self.stats.other_shares += qty.0,
                Event::Cancelled { remaining, .. } => self.stats.unfilled_shares += remaining.0,
                _ => {}
            }
        }
        if done {
            self.live.remove(&order_ref);
            if self.open.contains_key(&named) {
                self.stats.resyncs += 1;
                self.send(Command::Cancel { id: named }, out);
            }
        }
    }

    /// `X` or `C`: NASDAQ's order shrinks by `shares`. Ours goes down to NASDAQ's new
    /// size, never up: increasing would lose priority (D11) for an order NASDAQ didn't touch.
    fn reduce(&mut self, order_ref: u64, shares: u32, out: &mut Vec<Command>) {
        let Some(live) = self.live.get_mut(&order_ref) else {
            self.stats.untracked += 1;
            return;
        };
        live.shares = live.shares.saturating_sub(u64::from(shares));
        let (id, price, left) = (live.id, live.price, live.shares);
        if left == 0 {
            self.live.remove(&order_ref);
        }
        let Some(&open) = self.open.get(&id) else {
            self.stats.gone += 1;
            return;
        };
        if left == 0 {
            self.send(Command::Cancel { id }, out);
        } else if left < open {
            self.send(
                Command::Modify {
                    id,
                    qty: Qty(left),
                    price,
                },
                out,
            );
        }
    }

    fn delete(&mut self, order_ref: u64, out: &mut Vec<Command>) {
        let Some(live) = self.live.remove(&order_ref) else {
            self.stats.untracked += 1;
            return;
        };
        if self.open.contains_key(&live.id) {
            self.send(Command::Cancel { id: live.id }, out);
        } else {
            self.stats.gone += 1;
        }
    }

    fn new_id(&mut self) -> OrderId {
        self.next_id += 1;
        OrderId(self.next_id - 1)
    }

    /// Apply `cmd` to our book, keep its events in `self.events`, and track open sizes.
    fn send(&mut self, cmd: Command, out: &mut Vec<Command>) {
        out.push(cmd);
        self.stats.commands += 1;
        self.events.clear();
        self.book.apply(&cmd, &mut self.events);
        for e in &self.events {
            match *e {
                Event::Trade {
                    taker, maker, qty, ..
                } => {
                    for id in [taker, maker] {
                        if let Some(open) = self.open.get_mut(&id) {
                            *open -= qty.0;
                            if *open == 0 {
                                self.open.remove(&id);
                            }
                        }
                    }
                }
                Event::Modified { id, qty, .. } => {
                    self.open.insert(id, qty.0);
                }
                Event::Cancelled { id, .. } => {
                    self.open.remove(&id);
                }
                Event::Rejected { id, .. } => {
                    self.stats.rejects += 1;
                    // A rejected modify leaves the order as it was; a rejected add never rested.
                    if !matches!(cmd, Command::Modify { .. }) {
                        self.open.remove(&id);
                    }
                }
                Event::Accepted { .. } => {}
            }
        }
    }
}
