//! ITCH parser + book against a naive model, over random but valid message streams (D38),
//! and the whole sample day when it's present (`cargo test --release -- --ignored`).

use std::collections::BTreeMap;
use std::path::Path;

use lob::itch::{self, Body, Header, Message, Reader, Stock};
use lob::itch_book::ItchBook;
use lob::rng::Rng;
use lob::Side;

const SYMBOLS: u16 = 4;

/// The naive model: every live order in a sorted map, depth computed by scanning it.
#[derive(Default)]
struct Model {
    orders: BTreeMap<u64, (u16, Side, u32, u32)>,
}

impl Model {
    /// (price, shares, orders) per level, best first.
    fn depth(&self, locate: u16, side: Side) -> Vec<(u32, u64, u32)> {
        let mut levels: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
        for &(l, s, price, shares) in self.orders.values() {
            if l == locate && s == side {
                let e = levels.entry(price).or_default();
                e.0 += u64::from(shares);
                e.1 += 1;
            }
        }
        let v = levels.into_iter().map(|(p, (s, n))| (p, s, n));
        match side {
            Side::Buy => v.rev().collect(),
            Side::Sell => v.collect(),
        }
    }
}

fn stock(locate: u16) -> Stock {
    Stock::new(&format!("SYM{locate}"))
}

fn at(locate: u16, timestamp: u64, body: Body) -> Message {
    Message {
        header: Header {
            locate,
            tracking: 0,
            timestamp,
        },
        body,
    }
}

/// A valid day: directory and trading-state messages, then random order flow that only
/// ever refers to live orders, applied to `model` as it's generated.
fn session(seed: u64, n: usize, model: &mut Model) -> Vec<Message> {
    let mut rng = Rng::new(seed);
    let mut msgs = vec![at(0, 0, Body::SystemEvent { code: b'O' })];
    for l in 1..=SYMBOLS {
        msgs.push(at(l, 0, Body::StockDirectory { stock: stock(l) }));
        msgs.push(at(
            l,
            0,
            Body::TradingAction {
                stock: stock(l),
                state: b'T',
            },
        ));
    }
    msgs.push(at(0, 0, Body::SystemEvent { code: b'Q' }));
    let mut next_ref = 1u64;
    for t in 1..=n as u64 {
        let live: Vec<u64> = model.orders.keys().copied().collect();
        let pick = |rng: &mut Rng| live[rng.below(live.len() as u64) as usize];
        let roll = if live.is_empty() { 0 } else { rng.below(100) };
        let body = match roll {
            0..=44 => {
                let locate = 1 + rng.below(SYMBOLS as u64) as u16;
                let side = if rng.chance(50) {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let shares = rng.range(1, 500) as u32;
                // Crossing prices are fine: the book replays, it doesn't match.
                let price = rng.range(9_900, 10_100) as u32;
                let order_ref = next_ref;
                // Gaps in the references, as on a real feed (they're shared by all symbols).
                next_ref += 1 + rng.below(3);
                model
                    .orders
                    .insert(order_ref, (locate, side, price, shares));
                msgs.push(at(
                    locate,
                    t,
                    Body::AddOrder {
                        order_ref,
                        side,
                        shares,
                        stock: stock(locate),
                        price,
                    },
                ));
                continue;
            }
            45..=64 => {
                let r = pick(&mut rng);
                let left = model.orders[&r].3;
                let shares = rng.range(1, left as i64) as u32;
                if rng.chance(50) {
                    Body::Executed {
                        order_ref: r,
                        shares,
                        match_number: t,
                    }
                } else {
                    Body::ExecutedWithPrice {
                        order_ref: r,
                        shares,
                        match_number: t,
                        printable: true,
                        price: 1,
                    }
                }
            }
            65..=79 => {
                let r = pick(&mut rng);
                let shares = rng.range(1, model.orders[&r].3 as i64) as u32;
                Body::Cancel {
                    order_ref: r,
                    shares,
                }
            }
            80..=89 => Body::Delete {
                order_ref: pick(&mut rng),
            },
            _ => {
                let old_ref = pick(&mut rng);
                let new_ref = next_ref;
                next_ref += 1;
                Body::Replace {
                    old_ref,
                    new_ref,
                    shares: rng.range(1, 500) as u32,
                    price: rng.range(9_900, 10_100) as u32,
                }
            }
        };
        // Apply to the model.
        let locate = match body {
            Body::Executed {
                order_ref, shares, ..
            }
            | Body::ExecutedWithPrice {
                order_ref, shares, ..
            }
            | Body::Cancel { order_ref, shares } => {
                let o = model.orders.get_mut(&order_ref).unwrap();
                o.3 -= shares;
                let l = o.0;
                if o.3 == 0 {
                    model.orders.remove(&order_ref);
                }
                l
            }
            Body::Delete { order_ref } => model.orders.remove(&order_ref).unwrap().0,
            Body::Replace {
                old_ref,
                new_ref,
                shares,
                price,
            } => {
                let (l, side, _, _) = model.orders.remove(&old_ref).unwrap();
                model.orders.insert(new_ref, (l, side, price, shares));
                l
            }
            _ => unreachable!(),
        };
        msgs.push(at(locate, t, body));
    }
    msgs.push(at(0, n as u64 + 1, Body::SystemEvent { code: b'M' }));
    msgs
}

#[test]
fn random_sessions_match_the_naive_model_through_the_wire_format() {
    for seed in 0..20 {
        let mut model = Model::default();
        let msgs = session(seed, 3_000, &mut model);
        let mut bytes = Vec::new();
        for m in &msgs {
            itch::encode(m, &mut bytes);
        }
        let mut reader = Reader::new(&bytes[..]);
        let mut book = ItchBook::new();
        let mut i = 0;
        while let Some(m) = reader.next_message().unwrap() {
            assert_eq!(m, msgs[i], "seed {seed}: message {i} decoded differently");
            book.apply(&m)
                .unwrap_or_else(|e| panic!("seed {seed}: message {i}: {e}"));
            if i % 97 == 0 {
                book.check_invariants().unwrap();
            }
            i += 1;
        }
        assert_eq!(i, msgs.len());
        book.check_invariants().unwrap();
        assert_eq!(book.stats().live_orders, model.orders.len());
        for l in 1..=SYMBOLS {
            let s = book.symbol(l).unwrap();
            assert_eq!(s.stock, stock(l));
            for side in [Side::Buy, Side::Sell] {
                let got: Vec<_> = s
                    .depth(side, usize::MAX)
                    .into_iter()
                    .map(|(p, lv)| (p, lv.shares, lv.orders))
                    .collect();
                assert_eq!(got, model.depth(l, side), "seed {seed} locate {l} {side:?}");
            }
        }
    }
}

/// The sample day (D35), if it's been downloaded. Slow: run with `--release -- --ignored`.
#[test]
#[ignore]
fn sample_day_replays_without_errors() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/07302019.NASDAQ_ITCH50.gz");
    if !path.exists() {
        eprintln!("skipped: {} isn't there", path.display());
        return;
    }
    let mut reader = itch::open(&path).unwrap();
    let mut book = ItchBook::with_capacity(1 << 22);
    while let Some(m) = reader.next_message().unwrap() {
        if let Err(e) = book.apply(&m) {
            panic!("message {}: {e} ({m:?})", reader.count() - 1);
        }
    }
    book.check_invariants().unwrap();
    let s = book.stats();
    eprintln!("{s:?}");
    assert!(
        s.messages > 100_000_000,
        "a full day is hundreds of millions"
    );
}
