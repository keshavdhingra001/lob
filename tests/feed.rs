//! The market data feed (D40–D44) against the book it describes. After every command, the
//! publisher's level updates must be exactly the change in the book's depth, its snapshot
//! must equal the book's depth, and the feed must be the same behind either book.

mod common;

use common::random_command;
use std::collections::HashMap;

use lob::book::Level;
use lob::consumer::{Consumer, Link};
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

/// Publish `commands` over a lossy link. Whenever the consumer says its book is consistent,
/// it must be the engine's book as of the consumer's sequence number: maybe not the latest
/// (a lost last message shows only at the next heartbeat), but never one that didn't exist.
/// Returns the consumer's stats and how many commands it was consistent and up to date after.
fn lossy(
    seed: u64,
    commands: impl Iterator<Item = Command>,
    mut consumer: Consumer,
    drop_pct: u64,
) -> (lob::consumer::Stats, usize) {
    let mut book = FastBook::with_config(EDGE);
    let mut publisher = Publisher::new();
    let mut link = Link::new(seed, drop_pct, 5);
    if !consumer.is_live() {
        link.request_snapshot(&publisher);
    }
    // The engine's depth after each sequence number that ended a command.
    let mut history = HashMap::from([(0, depth(&book))]);
    let (mut events, mut msgs) = (Vec::new(), Vec::new());
    let mut current = 0;
    let consumer_depth = |c: &Consumer| [Side::Buy, Side::Sell].map(|s| c.depth(s, usize::MAX));
    for (n, cmd) in commands.enumerate() {
        events.clear();
        msgs.clear();
        book.apply(&cmd, &mut events);
        publisher.on_command(&cmd, &events, &mut msgs).unwrap();
        history.insert(publisher.seq(), depth(&book));
        link.deliver(&publisher, &msgs, &mut consumer);
        if consumer.is_consistent() {
            let want = history
                .get(&consumer.seq())
                .unwrap_or_else(|| panic!("seed {seed}, command #{n}: mid-command seq"));
            assert_eq!(
                &consumer_depth(&consumer),
                want,
                "seed {seed}, command #{n}"
            );
            current += (consumer.seq() == publisher.seq()) as usize;
        }
    }
    link.settle(&publisher, &mut consumer);
    assert!(consumer.is_consistent());
    assert_eq!(
        consumer_depth(&consumer),
        depth(&book),
        "seed {seed}, at the end"
    );
    (consumer.stats(), current)
}

#[test]
fn a_consumer_recovers_from_loss_and_duplicates() {
    for seed in 0..10 {
        let n = 3_000;
        let (stats, checked) = lossy(seed, random(seed, n, 1), Consumer::new(), 3);
        // Guard against a test that stopped testing: loss, recovery and checks all happen.
        assert!(
            stats.gaps > 10 && stats.snapshots > 10,
            "seed {seed}: {stats:?}"
        );
        assert!(stats.duplicates > 10, "seed {seed}: {stats:?}");
        assert!(
            checked > n / 2,
            "seed {seed}: up to date after only {checked} commands"
        );
    }
}

#[test]
fn duplicates_alone_need_no_snapshot() {
    let (stats, checked) = lossy(1, random(1, 3_000, 1), Consumer::new(), 0);
    assert_eq!((stats.gaps, stats.snapshots), (0, 0));
    assert_eq!(checked, 3_000);
    // With no snapshots, every duplicate is one the link sent twice.
    assert!(stats.duplicates > 50, "{stats:?}");
}

#[test]
fn a_late_joiner_catches_up() {
    let (stats, checked) = lossy(2, random(2, 3_000, 1), Consumer::late_joiner(), 0);
    assert_eq!((stats.gaps, stats.snapshots), (0, 1));
    assert!(checked > 2_990);
}
