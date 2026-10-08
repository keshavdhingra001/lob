//! Microbenchmarks (D28): add, cancel and match at fixed book depths, both books.
//!
//! Each benchmark keeps the book near its target depth. Ops are timed in chunks of up to
//! 100, so one pair of clock reads is spread over many ops. Each chunk is undone untimed
//! (adds cancelled, cancels and fills replaced), so the depth stays between `depth` and
//! `depth + chunk`. After each chunk an untimed assert checks the last op did what the
//! benchmark claims (rested, cancelled, traded), so a broken fixture can't time rejects.
//!
//! Book shape: `depth` orders split over both sides, 50 levels per side (fewer when
//! there aren't enough orders), qty 1 each. At 100k orders that's 1,000 per level, the
//! deep-queue case where the reference book's O(level) cancel shows.

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use lob::rng::Rng;
use lob::{Command, FastBook, OrderBook, OrderId, Price, Qty, RefBook, Side, TimeInForce};

const DEPTHS: [usize; 3] = [10, 1_000, 100_000];
const LEVELS: i64 = 50;
const MID: i64 = 10_000;

/// A book at a fixed depth plus what's needed to keep it there.
struct Fixture<B> {
    book: B,
    events: Vec<lob::Event>,
    /// Every resting order: (id, side, price).
    live: Vec<(OrderId, Side, Price)>,
    next_id: u64,
    rng: Rng,
    levels: i64,
}

impl<B: OrderBook> Fixture<B> {
    fn new(depth: usize) -> Self {
        let mut f = Fixture {
            book: B::with_config(Default::default()),
            events: Vec::with_capacity(64),
            live: Vec::with_capacity(depth + 128),
            next_id: 1,
            rng: Rng::new(7),
            levels: LEVELS.min((depth as i64 / 2).max(1)),
        };
        for i in 0..depth {
            let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
            let level = (i as i64 / 2) % f.levels;
            f.rest(side, level);
        }
        f
    }

    fn apply(&mut self, cmd: Command) {
        self.events.clear();
        self.book.apply(&cmd, &mut self.events);
    }

    /// Price of the `level`-th level from the touch on `side`. Bids at 9999 and below,
    /// asks at 10001 and above, so resting orders never cross.
    fn price(side: Side, level: i64) -> Price {
        match side {
            Side::Buy => Price(MID - 1 - level),
            Side::Sell => Price(MID + 1 + level),
        }
    }

    fn rest_cmd(&mut self, side: Side, level: i64) -> (Command, (OrderId, Side, Price)) {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        let price = Self::price(side, level);
        let cmd = Command::Limit {
            id,
            side,
            qty: Qty(1),
            price,
            tif: TimeInForce::Gtc,
            peak: None,
            stp: None,
        };
        (cmd, (id, side, price))
    }

    fn rest(&mut self, side: Side, level: i64) {
        let (cmd, order) = self.rest_cmd(side, level);
        self.apply(cmd);
        self.live.push(order);
    }

    fn random_side(&mut self) -> Side {
        if self.rng.chance(50) {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    /// Time `n` new resting orders at random levels, then cancel them untimed.
    fn add_chunk(&mut self, n: usize) -> Duration {
        let cmds: Vec<_> = (0..n)
            .map(|_| {
                let side = self.random_side();
                let level = self.rng.below(self.levels as u64) as i64;
                self.rest_cmd(side, level)
            })
            .collect();
        let start = Instant::now();
        for (cmd, _) in &cmds {
            self.events.clear();
            self.book.apply(cmd, &mut self.events);
        }
        let elapsed = start.elapsed();
        assert_eq!(self.events.len(), 1, "add must rest: {:?}", self.events);
        for (_, (id, _, _)) in cmds {
            self.apply(Command::Cancel { id });
        }
        elapsed
    }

    /// Add `n` orders untimed, then time cancelling `n` random resting orders (not the
    /// new ones in particular), so cancels hit the front, middle and back of queues.
    fn cancel_chunk(&mut self, n: usize) -> Duration {
        for _ in 0..n {
            let side = self.random_side();
            let level = self.rng.below(self.levels as u64) as i64;
            self.rest(side, level);
        }
        let cmds: Vec<Command> = (0..n)
            .map(|_| {
                let i = self.rng.below(self.live.len() as u64) as usize;
                let (id, _, _) = self.live.swap_remove(i);
                Command::Cancel { id }
            })
            .collect();
        let start = Instant::now();
        for cmd in &cmds {
            self.events.clear();
            self.book.apply(cmd, &mut self.events);
        }
        let elapsed = start.elapsed();
        assert!(
            matches!(self.events[..], [lob::Event::Cancelled { .. }]),
            "cancel must hit: {:?}",
            self.events
        );
        elapsed
    }

    /// Time `n` marketable IOC orders of qty 1, alternating sides, each filling one maker
    /// at the touch. Then replace the filled makers untimed, at the back of the touch level.
    /// Alternating means a side loses at most `n / 2` makers per chunk, so with `n` capped
    /// at the depth, a side never runs dry and every taker really matches.
    fn match_chunk(&mut self, n: usize) -> Duration {
        let cmds: Vec<Command> = (0..n)
            .map(|i| {
                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                let id = OrderId(self.next_id);
                self.next_id += 1;
                Command::Limit {
                    id,
                    side,
                    qty: Qty(1),
                    // Priced through the whole opposite side: always fills at its best.
                    price: Self::price(side.opposite(), self.levels),
                    tif: TimeInForce::Ioc,
                    peak: None,
                    stp: None,
                }
            })
            .collect();
        let start = Instant::now();
        for cmd in &cmds {
            self.events.clear();
            self.book.apply(cmd, &mut self.events);
        }
        let elapsed = start.elapsed();
        assert!(
            matches!(self.events[..], [_, lob::Event::Trade { .. }]),
            "match must fill one maker: {:?}",
            self.events
        );
        // The match benchmark never cancels, so replacements needn't be tracked in `live`.
        for cmd in cmds {
            if let Command::Limit { side, .. } = cmd {
                let (rest, _) = self.rest_cmd(side.opposite(), 0);
                self.apply(rest);
            }
        }
        elapsed
    }
}

/// Run `op` in chunks until `iters` ops are timed; return their total time.
fn chunked(iters: u64, chunk: usize, mut op: impl FnMut(usize) -> Duration) -> Duration {
    let mut total = Duration::ZERO;
    let mut left = iters as usize;
    while left > 0 {
        let n = left.min(chunk);
        total += op(n);
        left -= n;
    }
    total
}

fn bench_book<B: OrderBook>(c: &mut Criterion, name: &str) {
    let mut group = c.benchmark_group(name);
    for depth in DEPTHS {
        let chunk = depth.clamp(1, 100);
        group.bench_with_input(BenchmarkId::new("add", depth), &depth, |b, &depth| {
            let mut f = Fixture::<B>::new(depth);
            b.iter_custom(|iters| chunked(iters, chunk, |n| f.add_chunk(n)));
        });
        group.bench_with_input(BenchmarkId::new("cancel", depth), &depth, |b, &depth| {
            let mut f = Fixture::<B>::new(depth);
            b.iter_custom(|iters| chunked(iters, chunk, |n| f.cancel_chunk(n)));
        });
        group.bench_with_input(BenchmarkId::new("match", depth), &depth, |b, &depth| {
            let mut f = Fixture::<B>::new(depth);
            b.iter_custom(|iters| chunked(iters, chunk, |n| f.match_chunk(n)));
        });
    }
    group.finish();
}

fn benches(c: &mut Criterion) {
    bench_book::<RefBook>(c, "reference");
    bench_book::<FastBook>(c, "fast");
}

criterion_group!(book, benches);
criterion_main!(book);
