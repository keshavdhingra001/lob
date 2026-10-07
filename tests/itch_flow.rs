//! ITCH flow -> engine commands (D54): each message type by hand, then random
//! non-crossing streams checked against a naive model of NASDAQ's book.

use std::collections::BTreeMap;

use lob::book::apply_all;
use lob::itch::{Body, Header, Message, Stock};
use lob::itch_flow::{FlowStats, Translator};
use lob::rng::Rng;
use lob::{Command, Event, FastBook, Level, OrderBook, OrderId, Price, Qty, RefBook, Side};
use lob::{TimeInForce::Gtc, TimeInForce::Ioc};

const OURS: u16 = 7;

fn msg(locate: u16, body: Body) -> Message {
    Message {
        header: Header {
            locate,
            tracking: 0,
            timestamp: 0,
        },
        body,
    }
}

fn add(order_ref: u64, side: Side, shares: u32, price: u32) -> Message {
    msg(
        OURS,
        Body::AddOrder {
            order_ref,
            side,
            shares,
            stock: Stock::new("SYM"),
            price,
        },
    )
}

fn exec(order_ref: u64, shares: u32) -> Message {
    msg(
        OURS,
        Body::Executed {
            order_ref,
            shares,
            match_number: 0,
        },
    )
}

/// A translator that has seen the directory entries for SYM (ours) and OTHER.
fn translator() -> Translator {
    let mut t = Translator::new(Stock::new("SYM"));
    let mut out = Vec::new();
    for (locate, name) in [(OURS, "SYM"), (OURS + 1, "OTHER")] {
        let stock = Stock::new(name);
        t.on_message(&msg(locate, Body::StockDirectory { stock }), &mut out);
    }
    assert!(out.is_empty());
    t
}

fn feed(t: &mut Translator, msgs: &[Message]) -> Vec<Command> {
    let mut out = Vec::new();
    for m in msgs {
        t.on_message(m, &mut out);
    }
    out
}

fn limit(id: u64, side: Side, qty: u64, price: i64, tif: lob::TimeInForce) -> Command {
    Command::Limit {
        id: OrderId(id),
        side,
        qty: Qty(qty),
        price: Price(price),
        tif,
    }
}

#[test]
fn adds_become_limits_in_cents_and_other_symbols_are_ignored() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(50, Side::Buy, 100, 1_002_300),
            add(51, Side::Sell, 10, 1_002_350), // sub-penny
            msg(
                OURS + 1,
                Body::AddOrder {
                    order_ref: 52,
                    side: Side::Sell,
                    shares: 5,
                    stock: Stock::new("OTHER"),
                    price: 1_000_000,
                },
            ),
            msg(OURS, Body::Delete { order_ref: 51 }),
        ],
    );
    assert_eq!(out, [limit(1, Side::Buy, 100, 10_023, Gtc)]);
    let s = t.stats();
    assert_eq!((s.messages, s.adds, s.sub_penny, s.untracked), (3, 2, 1, 1));
}

#[test]
fn an_execution_of_the_queue_head_fills_the_named_order() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(10, Side::Sell, 100, 1_000_000),
            add(11, Side::Sell, 50, 1_000_000),
            exec(10, 100),
        ],
    );
    // The fill finishes order 10 in both books, so no resync is needed.
    assert_eq!(out[2], limit(3, Side::Buy, 100, 10_000, Ioc));
    assert_eq!(out.len(), 3);
    let s = t.stats();
    assert_eq!((s.named_shares, s.other_shares, s.resyncs), (100, 0, 0));
}

#[test]
fn an_execution_out_of_queue_order_fills_the_head_then_resyncs() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(10, Side::Buy, 30, 990_000),
            add(11, Side::Buy, 30, 990_000),
            // NASDAQ fills order 11, which is behind order 10 in our queue.
            exec(11, 30),
            msg(OURS, Body::Delete { order_ref: 10 }),
        ],
    );
    // Our IOC fills order 10 (id 1). NASDAQ says 11 (id 2) is done, so it's cancelled;
    // NASDAQ's later delete of 10 finds nothing left in our book.
    assert_eq!(
        out[2..],
        [
            limit(3, Side::Sell, 30, 9_900, Ioc),
            Command::Cancel { id: OrderId(2) }
        ]
    );
    let s = t.stats();
    assert_eq!((s.named_shares, s.other_shares), (0, 30));
    assert_eq!((s.resyncs, s.gone), (1, 1));
    assert!(t.book().depth(Side::Buy, usize::MAX).is_empty());
}

#[test]
fn partial_cancels_only_reduce() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(10, Side::Buy, 40, 990_000),
            add(11, Side::Buy, 100, 990_000),
            exec(11, 30), // our IOC takes 30 from order 10 instead: 10 has 10 left, 11 has 100
            msg(
                OURS,
                Body::Cancel {
                    order_ref: 10,
                    shares: 15,
                },
            ), // NASDAQ: 10 has 25 left. Ours has 10, so nothing to do.
            msg(
                OURS,
                Body::Cancel {
                    order_ref: 11,
                    shares: 20,
                },
            ), // NASDAQ: 11 has 50 left. Ours has 100: down to 50.
            msg(
                OURS,
                Body::ExecutedWithPrice {
                    order_ref: 11,
                    shares: 50,
                    match_number: 0,
                    printable: true,
                    price: 990_100,
                },
            ), // a cross execution finishes 11
        ],
    );
    assert_eq!(
        out[3..],
        [
            Command::Modify {
                id: OrderId(2),
                qty: Qty(50),
                price: Price(9_900)
            },
            Command::Cancel { id: OrderId(2) },
        ]
    );
    assert_eq!(t.stats().cross_executions, 1);
}

#[test]
fn a_replace_is_a_cancel_and_a_new_order_with_the_old_side() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(10, Side::Sell, 40, 1_000_000),
            msg(
                OURS,
                Body::Replace {
                    old_ref: 10,
                    new_ref: 12,
                    shares: 60,
                    price: 1_000_100,
                },
            ),
            msg(OURS, Body::Delete { order_ref: 12 }),
        ],
    );
    assert_eq!(
        out[1..],
        [
            Command::Cancel { id: OrderId(1) },
            limit(2, Side::Sell, 60, 10_001, Gtc),
            Command::Cancel { id: OrderId(2) },
        ]
    );
}

/// NASDAQ's book for one symbol: every live order, oldest first per level.
#[derive(Default)]
struct Model {
    orders: BTreeMap<u64, (Side, u32, u32)>,
}

impl Model {
    fn depth(&self, side: Side) -> Vec<Level> {
        let mut levels: BTreeMap<u32, Level> = BTreeMap::new();
        for &(s, price, shares) in self.orders.values() {
            if s == side {
                let l = levels.entry(price).or_insert(Level {
                    price: Price(i64::from(price / 100)),
                    qty: Qty(0),
                    orders: 0,
                });
                l.qty.0 += u64::from(shares);
                l.orders += 1;
            }
        }
        match side {
            Side::Buy => levels.into_values().rev().collect(),
            Side::Sell => levels.into_values().collect(),
        }
    }

    /// The live orders at `side`'s best price, oldest first.
    fn best_queue(&self, side: Side) -> Vec<u64> {
        let best = self.depth(side).first().map(|l| l.price.0 as u32 * 100);
        self.orders
            .iter()
            .filter(|(_, &(s, p, _))| s == side && Some(p) == best)
            .map(|(&r, _)| r)
            .collect()
    }
}

/// Random flow that never crosses: bids at $99.00–$99.99, asks at $100.00–$100.99, and
/// executions only at the best price, like NASDAQ's displayed book (D38). With `fifo`,
/// each execution names the oldest order at the touch; otherwise any order there.
/// After each message, `check` gets the model and the translator.
fn random_day(seed: u64, n: usize, fifo: bool, mut check: impl FnMut(&Model, &Translator)) {
    let mut rng = Rng::new(seed);
    let mut model = Model::default();
    let mut t = translator();
    let mut out = Vec::new();
    let mut next_ref = 1;
    let mut all = Vec::new();
    for i in 0..n + 1 {
        let live: Vec<u64> = model.orders.keys().copied().collect();
        let roll = if live.is_empty() { 0 } else { rng.below(100) };
        // The last step deletes everything left, so NASDAQ's book ends empty.
        let msgs: Vec<Message> = if i == n {
            live.iter()
                .map(|&order_ref| msg(OURS, Body::Delete { order_ref }))
                .collect()
        } else {
            let pick = live
                .get(rng.below(live.len().max(1) as u64) as usize)
                .copied();
            vec![match roll {
                0..=44 => {
                    let side = if rng.chance(50) {
                        Side::Buy
                    } else {
                        Side::Sell
                    };
                    let base = if side == Side::Buy {
                        990_000
                    } else {
                        1_000_000
                    };
                    next_ref += 1;
                    add(
                        next_ref,
                        side,
                        1 + rng.below(200) as u32,
                        base + 100 * rng.below(100) as u32,
                    )
                }
                45..=64 => {
                    let side = if rng.chance(50) {
                        Side::Buy
                    } else {
                        Side::Sell
                    };
                    let queue = model.best_queue(side);
                    if queue.is_empty() {
                        continue;
                    }
                    let r = if fifo {
                        queue[0]
                    } else {
                        queue[rng.below(queue.len() as u64) as usize]
                    };
                    let shares = model.orders[&r].2;
                    exec(r, 1 + rng.below(u64::from(shares)) as u32)
                }
                65..=79 => {
                    let r = pick.unwrap();
                    let shares = model.orders[&r].2;
                    let cut = 1 + rng.below(u64::from(shares)) as u32;
                    msg(
                        OURS,
                        Body::Cancel {
                            order_ref: r,
                            shares: cut,
                        },
                    )
                }
                80..=89 => {
                    let r = pick.unwrap();
                    let (side, _, _) = model.orders[&r];
                    let base = if side == Side::Buy {
                        990_000
                    } else {
                        1_000_000
                    };
                    next_ref += 1;
                    msg(
                        OURS,
                        Body::Replace {
                            old_ref: r,
                            new_ref: next_ref,
                            shares: 1 + rng.below(200) as u32,
                            price: base + 100 * rng.below(100) as u32,
                        },
                    )
                }
                _ => msg(
                    OURS,
                    Body::Delete {
                        order_ref: pick.unwrap(),
                    },
                ),
            }]
        };
        for m in msgs {
            match m.body {
                Body::AddOrder {
                    order_ref,
                    side,
                    shares,
                    price,
                    ..
                } => {
                    model.orders.insert(order_ref, (side, price, shares));
                }
                Body::Executed {
                    order_ref, shares, ..
                }
                | Body::Cancel { order_ref, shares } => {
                    let o = model.orders.get_mut(&order_ref).unwrap();
                    o.2 -= shares;
                    if o.2 == 0 {
                        model.orders.remove(&order_ref);
                    }
                }
                Body::Delete { order_ref } => {
                    model.orders.remove(&order_ref);
                }
                Body::Replace {
                    old_ref,
                    new_ref,
                    shares,
                    price,
                } => {
                    let (side, _, _) = model.orders.remove(&old_ref).unwrap();
                    model.orders.insert(new_ref, (side, price, shares));
                }
                _ => unreachable!(),
            }
            let start = out.len();
            t.on_message(&m, &mut out);
            all.extend_from_slice(&out[start..]);
            check(&model, &t);
        }
    }
    // Replaying the journal through either book gives the translator's book.
    for depth in [replay::<RefBook>(&all), replay::<FastBook>(&all)] {
        assert_eq!(depth, both(t.book()));
    }
    assert!(model.orders.is_empty());
    assert_eq!(
        both(t.book()),
        (vec![], vec![]),
        "NASDAQ's book is empty, so ours must be"
    );
}

fn both<B: OrderBook>(book: &B) -> (Vec<Level>, Vec<Level>) {
    (
        book.depth(Side::Buy, usize::MAX),
        book.depth(Side::Sell, usize::MAX),
    )
}

fn replay<B: OrderBook>(commands: &[Command]) -> (Vec<Level>, Vec<Level>) {
    let mut book = B::with_config(Default::default());
    apply_all(&mut book, commands);
    both(&book)
}

#[test]
fn with_fifo_executions_our_book_is_nasdaqs_after_every_message() {
    for seed in 1..=20 {
        let mut last = FlowStats::default();
        random_day(seed, 2_000, true, |model, t| {
            assert_eq!(
                both(t.book()),
                (model.depth(Side::Buy), model.depth(Side::Sell))
            );
            last = *t.stats();
        });
        assert!(last.executions > 100, "seed {seed}: {last:?}");
        assert_eq!(
            last.named_shares,
            last.named_shares + last.other_shares + last.unfilled_shares
        );
        assert_eq!(
            (last.resyncs, last.gone, last.adds_traded, last.rejects),
            (0, 0, 0, 0)
        );
    }
}

#[test]
fn with_executions_anywhere_at_the_touch_our_levels_are_nasdaqs_and_end_empty() {
    let mut drifted = 0;
    for seed in 1..=20 {
        let mut last = FlowStats::default();
        random_day(seed, 2_000, false, |model, t| {
            // Sizes drift between orders at a level, but every order live in our book is
            // live in NASDAQ's, so our book never shows a price NASDAQ's doesn't.
            for side in [Side::Buy, Side::Sell] {
                let theirs = model.depth(side);
                for l in t.book().depth(side, usize::MAX) {
                    let n = theirs.iter().find(|m| m.price == l.price);
                    assert!(
                        n.is_some_and(|n| n.orders >= l.orders),
                        "seed {seed}: {l:?} vs {n:?}"
                    );
                }
            }
            last = *t.stats();
        });
        assert_eq!(last.rejects, 0);
        drifted += last.other_shares;
    }
    assert!(drifted > 0, "no execution ever named a non-head order");
}

#[test]
fn the_journal_replays_to_the_same_events() {
    let mut t = translator();
    let cmds = feed(
        &mut t,
        &[
            add(10, Side::Buy, 30, 990_000),
            add(11, Side::Buy, 30, 990_000),
            exec(11, 30),
        ],
    );
    let mut events = Vec::new();
    let mut book = RefBook::new();
    for c in &cmds {
        book.apply(c, &mut events);
    }
    assert!(events.contains(&Event::Trade {
        taker: OrderId(3),
        maker: OrderId(1),
        taker_side: Side::Sell,
        qty: Qty(30),
        price: Price(9_900),
    }));
}

#[test]
fn an_add_that_trades_or_is_rejected_leaves_nothing_to_cancel() {
    let mut t = translator();
    let out = feed(
        &mut t,
        &[
            add(10, Side::Buy, 50, 1_000_000),
            // Crosses in our book (as an auction leftover would) and fills completely.
            add(11, Side::Sell, 50, 1_000_000),
            // Over the default max_qty of 1,000,000: rejected.
            add(12, Side::Sell, 2_000_000, 1_010_000),
            msg(OURS, Body::Delete { order_ref: 10 }),
            msg(OURS, Body::Delete { order_ref: 11 }),
            msg(OURS, Body::Delete { order_ref: 12 }),
        ],
    );
    assert_eq!(out.len(), 3, "{out:?}");
    let s = t.stats();
    assert_eq!((s.adds_traded, s.rejects, s.gone), (1, 1, 3));
}
