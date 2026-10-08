//! Seeded synthetic order flow (D18).
//!
//! Not a market model, but shaped like real order flow where it matters for a book:
//! - the mid price takes a random walk, so levels are created and emptied all the time
//! - most orders are passive and land a few ticks from the mid, so queues build up at
//!   the touch, where matching happens
//! - cancels and modifies are frequent (on real venues most orders are cancelled, not filled)
//! - a minority of orders cross: aggressive limits, IOCs, FOKs and markets
//!
//! The generator can't see the book, so some cancels and modifies target orders that
//! already filled, and those come back `rejected unknown-order`, as late cancels do on
//! real venues.
//!
//! With `stp_groups` set, new orders also carry STP groups (D67), drawn from a few so
//! that self-trades are constantly attempted. With `iceberg_pct` set, that share of the
//! passive orders of 20 lots or more are icebergs showing a fifth (D89). The defaults have
//! neither, and draw exactly the numbers they always did, so the golden digest is unchanged.

use std::num::{NonZeroU16, NonZeroU64};

use crate::command::{Command, Stp, StpAction, TimeInForce};
use crate::rng::Rng;
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
pub struct GenConfig {
    pub seed: u64,
    /// Starting mid price, in ticks.
    pub start_mid: i64,
    /// Orders the generator tracks for cancels and modifies. Bounds its memory too.
    pub max_live: usize,
    /// New orders get a group in `1..=stp_groups`, or none, with equal odds. 0: never.
    pub stp_groups: u16,
    /// Percent of passive orders of 20+ lots that are icebergs. 0: none.
    pub iceberg_pct: u64,
}

impl Default for GenConfig {
    fn default() -> Self {
        GenConfig {
            seed: 1,
            start_mid: 10_000,
            max_live: 5_000,
            stp_groups: 0,
            iceberg_pct: 0,
        }
    }
}

/// An order the generator believes may still rest.
#[derive(Clone, Copy)]
struct Live {
    id: OrderId,
    side: Side,
    price: i64,
    qty: u64,
}

pub struct Generator {
    rng: Rng,
    mid: i64,
    next_id: u64,
    max_live: usize,
    stp_groups: u16,
    iceberg_pct: u64,
    live: Vec<Live>,
}

impl GenConfig {
    /// The default flow with another seed.
    pub fn with_seed(seed: u64) -> Self {
        GenConfig {
            seed,
            ..GenConfig::default()
        }
    }
}

impl Generator {
    /// The default flow with another seed.
    pub fn seeded(seed: u64) -> Self {
        Generator::new(GenConfig::with_seed(seed))
    }

    pub fn new(config: GenConfig) -> Self {
        Generator {
            rng: Rng::new(config.seed),
            mid: config.start_mid,
            next_id: 1,
            max_live: config.max_live,
            stp_groups: config.stp_groups,
            iceberg_pct: config.iceberg_pct,
            live: Vec::new(),
        }
    }

    fn fresh_id(&mut self) -> OrderId {
        self.next_id += 1;
        OrderId(self.next_id - 1)
    }

    fn side(&mut self) -> Side {
        if self.rng.chance(50) {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn stp(&mut self) -> Option<Stp> {
        random_stp(&mut self.rng, self.stp_groups)
    }

    /// Mostly small round lots, occasionally a large one.
    fn qty(&mut self) -> u64 {
        const LOTS: [u64; 8] = [1, 5, 10, 10, 20, 50, 100, 500];
        LOTS[self.rng.below(LOTS.len() as u64) as usize]
    }

    /// Ticks away from the mid on the passive side: skewed towards the touch.
    fn passive_offset(&mut self) -> i64 {
        let spread = self.rng.below(12) + 1;
        1 + self.rng.below(spread) as i64
    }

    fn passive_price(&mut self, side: Side) -> i64 {
        let offset = self.passive_offset();
        match side {
            Side::Buy => self.mid - offset,
            Side::Sell => self.mid + offset,
        }
    }

    /// A price up to 3 ticks through the mid: likely to trade.
    fn aggressive_price(&mut self, side: Side) -> i64 {
        let through = self.rng.below(4) as i64;
        match side {
            Side::Buy => self.mid + through,
            Side::Sell => self.mid - through,
        }
    }

    fn limit(&mut self, side: Side, price: i64, qty: u64, tif: TimeInForce) -> Command {
        self.order(side, price, qty, tif, None)
    }

    fn order(
        &mut self,
        side: Side,
        price: i64,
        qty: u64,
        tif: TimeInForce,
        peak: Option<NonZeroU64>,
    ) -> Command {
        let id = self.fresh_id();
        if matches!(tif, TimeInForce::Gtc | TimeInForce::PostOnly) {
            self.live.push(Live {
                id,
                side,
                price,
                qty,
            });
        }
        Command::Limit {
            id,
            side,
            qty: Qty(qty),
            price: Price(price),
            tif,
            peak,
            stp: self.stp(),
        }
    }

    fn passive(&mut self) -> Command {
        let side = self.side();
        let price = self.passive_price(side);
        let qty = self.qty();
        let tif = if self.rng.chance(10) {
            TimeInForce::PostOnly
        } else {
            TimeInForce::Gtc
        };
        // `&&` keeps the default flow's draws as they were.
        let iceberg = self.iceberg_pct > 0 && qty >= 20 && self.rng.chance(self.iceberg_pct);
        let peak = NonZeroU64::new(qty / 5).filter(|_| iceberg);
        self.order(side, price, qty, tif, peak)
    }

    fn cancel(&mut self) -> Command {
        let i = self.rng.below(self.live.len() as u64) as usize;
        let order = self.live.swap_remove(i);
        Command::Cancel { id: order.id }
    }

    fn modify(&mut self) -> Command {
        let i = self.rng.below(self.live.len() as u64) as usize;
        let order = &mut self.live[i];
        if self.rng.chance(50) {
            // Reduce in place: keeps priority.
            order.qty = (order.qty / 2).max(1);
        } else {
            let offset = 1 + self.rng.below(6) as i64;
            order.price = match order.side {
                Side::Buy => self.mid - offset,
                Side::Sell => self.mid + offset,
            };
            order.qty = LOTS_FOR_MODIFY[self.rng.below(4) as usize];
        }
        Command::Modify {
            id: order.id,
            qty: Qty(order.qty),
            price: Price(order.price),
        }
    }
}

/// A group in `1..=groups` with a random action, or none, with equal odds. When `groups`
/// is 0 it's always none and draws no number, so an ungrouped flow is what it always was.
pub fn random_stp(rng: &mut Rng, groups: u16) -> Option<Stp> {
    if groups == 0 {
        return None;
    }
    let group = NonZeroU16::new(rng.below(groups as u64 + 1) as u16)?;
    const ACTIONS: [StpAction; 3] = [
        StpAction::CancelNewest,
        StpAction::CancelOldest,
        StpAction::CancelBoth,
    ];
    let action = ACTIONS[rng.below(3) as usize];
    Some(Stp { group, action })
}

const LOTS_FOR_MODIFY: [u64; 4] = [1, 10, 20, 50];

impl Iterator for Generator {
    type Item = Command;

    /// Never ends; use `take(n)`.
    fn next(&mut self) -> Option<Command> {
        if self.rng.chance(10) {
            self.mid += if self.rng.chance(50) { 1 } else { -1 };
        }
        let roll = self.rng.below(100);
        let cmd = if self.live.len() >= self.max_live || (roll < 30 && !self.live.is_empty()) {
            self.cancel()
        } else if roll < 40 && !self.live.is_empty() {
            self.modify()
        } else if roll < 48 {
            let side = self.side();
            let price = self.aggressive_price(side);
            let qty = self.qty();
            let tif = match self.rng.below(4) {
                0 | 1 => TimeInForce::Ioc,
                2 => TimeInForce::Fok,
                _ => TimeInForce::Gtc,
            };
            self.limit(side, price, qty, tif)
        } else if roll < 52 {
            let id = self.fresh_id();
            let side = self.side();
            let qty = self.qty().min(50);
            Command::Market {
                id,
                side,
                qty: Qty(qty),
                stp: self.stp(),
            }
        } else {
            self.passive()
        };
        Some(cmd)
    }
}

/// Worst case for the reference book's cancel (D22): `n` buys queued at one price, then
/// cancelled in a seeded random order. Each reference cancel scans the queue (O(n)), so
/// the whole session is O(n²) there and O(n) in the fast book.
pub fn deep_queue(n: u64, seed: u64) -> Vec<Command> {
    let mut rng = Rng::new(seed);
    let mut ids: Vec<u64> = (1..=n).collect();
    let mut cmds: Vec<Command> = ids
        .iter()
        .map(|&id| Command::Limit {
            id: OrderId(id),
            side: Side::Buy,
            qty: Qty(1),
            price: Price(10_000),
            tif: TimeInForce::Gtc,
            peak: None,
            stp: None,
        })
        .collect();
    // Fisher-Yates shuffle, so cancels hit the front, middle and back of the queue.
    for i in (1..ids.len()).rev() {
        ids.swap(i, rng.below(i as u64 + 1) as usize);
    }
    cmds.extend(
        ids.into_iter()
            .map(|id| Command::Cancel { id: OrderId(id) }),
    );
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_flow() {
        let a: Vec<Command> = Generator::new(GenConfig::default()).take(5_000).collect();
        let b: Vec<Command> = Generator::new(GenConfig::default()).take(5_000).collect();
        assert_eq!(a, b);
        let c: Vec<Command> = Generator::seeded(2).take(5_000).collect();
        assert_ne!(a, c);
    }

    #[test]
    fn mix_has_every_command_kind() {
        let mut counts = [0usize; 4];
        for cmd in Generator::new(GenConfig::default()).take(20_000) {
            counts[match cmd {
                Command::Limit { .. } => 0,
                Command::Market { .. } => 1,
                Command::Modify { .. } => 2,
                Command::Cancel { .. } => 3,
            }] += 1;
        }
        // Roughly 56% limits, 4% markets, 10% modifies, 30% cancels.
        let pct: Vec<usize> = counts.iter().map(|c| c * 100 / 20_000).collect();
        assert!((50..=62).contains(&pct[0]), "{pct:?}");
        assert!((2..=6).contains(&pct[1]), "{pct:?}");
        assert!((7..=13).contains(&pct[2]), "{pct:?}");
        assert!((25..=35).contains(&pct[3]), "{pct:?}");
    }

    #[test]
    fn deep_queue_adds_then_cancels_everything_once() {
        let cmds = deep_queue(100, 3);
        assert_eq!(cmds.len(), 200);
        let mut cancelled: Vec<u64> = cmds[100..]
            .iter()
            .map(|c| match c {
                Command::Cancel { id } => id.0,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_ne!(cancelled, (1..=100).collect::<Vec<_>>(), "not shuffled");
        cancelled.sort();
        assert_eq!(cancelled, (1..=100).collect::<Vec<_>>());
    }

    #[test]
    fn live_set_is_bounded() {
        let mut g = Generator::new(GenConfig {
            max_live: 100,
            ..GenConfig::default()
        });
        for _ in 0..10_000 {
            g.next();
            assert!(g.live.len() <= 100);
        }
    }
}
