//! The market data feed (D40–D44) against the book it describes. After every command, the
//! publisher's level updates must be exactly the change in the book's depth, its snapshot
//! must equal the book's depth, and the feed must be the same behind either book.

mod common;

use common::random_command;
use lob::book::Level;
use lob::feed::{self, Msg, Publisher};
use lob::gen::Generator;
use lob::replay::Fnv64;
use lob::rng::Rng;
use lob::{BookConfig, Command, Event, FastBook, OrderBook, Price, Qty, RefBook, Side};

fn depth<B: OrderBook>(book: &B) -> [Vec<Level>; 2] {
    [Side::Buy, Side::Sell].map(|s| book.depth(s, usize::MAX))
}

/// The level updates a perfect feed sends for a change from `before` to `after`:
/// every level whose total changed, bids then asks, by price (D41).
fn diff(before: &[Vec<Level>; 2], after: &[Vec<Level>; 2]) -> Vec<(Side, Level)> {
    let mut out = Vec::new();
    for (i, side) in [Side::Buy, Side::Sell].into_iter().enumerate() {
        let mut prices: Vec<Price> = before[i].iter().chain(&after[i]).map(|l| l.price).collect();
        prices.sort();
        prices.dedup();
        let at = |levels: &[Level], p| {
            levels
                .iter()
                .find(|l| l.price == p)
                .copied()
                .unwrap_or(Level {
                    price: p,
                    qty: Qty(0),
                    orders: 0,
                })
        };
        for p in prices {
            let (b, a) = (at(&before[i], p), at(&after[i], p));
            if a != b {
                out.push((side, a));
            }
        }
    }
    out
}

/// Check one command's messages against the book's change.
fn check(
    cmd: &Command,
    events: &[Event],
    msgs: &[Msg],
    first_seq: u64,
    before: &[Vec<Level>; 2],
    after: &[Vec<Level>; 2],
) -> Result<(), String> {
    let trades: Vec<Msg> = events
        .iter()
        .filter_map(|e| match *e {
            Event::Trade {
                taker_side,
                qty,
                price,
                ..
            } => Some((taker_side, price, qty)),
            _ => None,
        })
        .zip(first_seq..)
        .map(|((aggressor, price, qty), seq)| Msg::Trade {
            seq,
            aggressor,
            price,
            qty,
            last: false,
        })
        .collect();
    let levels = diff(before, after)
        .into_iter()
        .zip(first_seq + trades.len() as u64..)
        .map(|((side, level), seq)| Msg::Level {
            seq,
            side,
            level,
            last: false,
        });
    let mut want: Vec<Msg> = trades.into_iter().chain(levels).collect();
    if let Some(Msg::Level { last, .. } | Msg::Trade { last, .. }) = want.last_mut() {
        *last = true;
    }
    if msgs != want {
        return Err(format!("`{cmd}`: sent {msgs:?}\nwanted {want:?}"));
    }
    Ok(())
}

/// Run `commands` through a book of type `B` and a publisher, checking every command.
/// Returns the digest of the encoded feed.
fn run<B: OrderBook>(config: BookConfig, commands: impl Iterator<Item = Command>) -> u64 {
    let mut book = B::with_config(config);
    let mut publisher = Publisher::new();
    let (mut events, mut msgs, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
    let mut hash = Fnv64::default();
    let mut before = depth(&book);
    for (n, cmd) in commands.enumerate() {
        events.clear();
        msgs.clear();
        book.apply(&cmd, &mut events);
        let first_seq = publisher.seq() + 1;
        publisher
            .on_command(&cmd, &events, &mut msgs)
            .unwrap_or_else(|e| panic!("command #{n}: {e}"));
        let after = depth(&book);
        check(&cmd, &events, &msgs, first_seq, &before, &after)
            .unwrap_or_else(|e| panic!("command #{n}: {e}"));
        let snap = publisher.snapshot();
        assert_eq!([snap.bids, snap.asks], after, "command #{n}: snapshot");
        assert_eq!(snap.seq, first_seq - 1 + msgs.len() as u64);
        before = after;
        bytes.clear();
        for msg in &msgs {
            feed::encode(msg, &mut bytes);
        }
        hash.update(&bytes);
    }
    hash.finish()
}

fn random(seed: u64, n: usize, tick: i64) -> impl Iterator<Item = Command> {
    let mut rng = Rng::new(seed);
    let mut next_id = 1;
    (0..n).map(move |_| random_command(&mut rng, &mut next_id, tick))
}

const EDGE: BookConfig = BookConfig {
    tick_size: 1,
    max_qty: 12,
};

#[test]
fn level_updates_are_exactly_the_depth_change() {
    for seed in 0..10 {
        let a = run::<RefBook>(EDGE, random(seed, 3_000, 1));
        let b = run::<FastBook>(EDGE, random(seed, 3_000, 1));
        assert_eq!(a, b, "seed {seed}: the two books' feeds differ");
    }
}

#[test]
fn generated_flow_feed_is_identical_on_both_books_and_pinned() {
    let config = BookConfig::default();
    let a = run::<RefBook>(config, Generator::seeded(1).take(20_000));
    let b = run::<FastBook>(config, Generator::seeded(1).take(20_000));
    assert_eq!(a, b);
    // Pinned like the event digest (D17): a change here changes the feed's bytes.
    assert_eq!(a, 0x1bef_8ebc_21b7_ca92);
}
