//! The receiving end of the feed (D42): applies level updates in sequence, notices a gap,
//! buffers while it waits for a snapshot, and resumes from the snapshot plus the buffer.

use std::collections::BTreeMap;

use crate::book::Level;
use crate::feed::{self, Msg, Publisher, Snapshot};
use crate::rng::Rng;
use crate::types::{Price, Side};

/// What the consumer needs from its caller after a message or snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    /// Its book can't be trusted until a snapshot arrives.
    RequestSnapshot,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Messages applied to the book, live or from the buffer.
    pub applied: u64,
    /// Messages already applied or already buffered, ignored.
    pub duplicates: u64,
    /// Gaps found, live or in the buffer after a snapshot.
    pub gaps: u64,
    pub snapshots: u64,
}

#[derive(Debug)]
pub struct Consumer {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
    /// The sequence number the book needs next.
    next: u64,
    /// Waiting for a snapshot. Messages go to `buffer` instead of the book.
    recovering: bool,
    /// Messages received while recovering, sequence numbers increasing. Unbounded here; a
    /// real consumer would cap it and drop the oldest, since the snapshot will cover them.
    buffer: Vec<Msg>,
    /// The last message applied ended a command (or a snapshot was just loaded).
    at_boundary: bool,
    stats: Stats,
}

impl Consumer {
    /// A consumer that has been listening since the first message.
    pub fn new() -> Self {
        Consumer {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            next: 1,
            recovering: false,
            buffer: Vec::new(),
            at_boundary: true,
            stats: Stats::default(),
        }
    }

    /// A consumer joining mid-stream: it buffers until its first snapshot. The caller asks for one.
    pub fn late_joiner() -> Self {
        Consumer {
            recovering: true,
            ..Consumer::new()
        }
    }

    pub fn on_msg(&mut self, msg: Msg) -> Action {
        let seq = msg.seq();
        if self.recovering {
            if self.buffer.last().is_some_and(|m| seq <= m.seq()) {
                self.stats.duplicates += 1;
            } else {
                self.buffer.push(msg);
            }
            return Action::Continue;
        }
        if seq < self.next {
            self.stats.duplicates += 1;
            Action::Continue
        } else if seq == self.next {
            self.apply(msg);
            Action::Continue
        } else {
            self.stats.gaps += 1;
            self.recovering = true;
            self.buffer.push(msg);
            Action::RequestSnapshot
        }
    }

    /// The publisher's last sequence number, sent when it has nothing else to say. Without
    /// it, losing the last message before a quiet spell goes unnoticed until the next one.
    pub fn on_heartbeat(&mut self, last_seq: u64) -> Action {
        if self.recovering || last_seq < self.next {
            return Action::Continue;
        }
        self.stats.gaps += 1;
        self.recovering = true;
        Action::RequestSnapshot
    }

    /// Load a snapshot, then catch up from the buffer. Ignored unless recovering.
    pub fn on_snapshot(&mut self, snap: &Snapshot) -> Action {
        if !self.recovering {
            return Action::Continue;
        }
        self.stats.snapshots += 1;
        let load = |levels: &[Level]| levels.iter().map(|l| (l.price, *l)).collect();
        self.bids = load(&snap.bids);
        self.asks = load(&snap.asks);
        self.next = snap.seq + 1;
        self.at_boundary = true;
        self.recovering = false;
        let mut buffer = std::mem::take(&mut self.buffer);
        for i in 0..buffer.len() {
            let msg = buffer[i];
            if msg.seq() < self.next {
                continue; // already in the snapshot
            }
            if msg.seq() > self.next {
                // Lost after the snapshot was taken: keep the rest and wait for a newer one.
                self.stats.gaps += 1;
                self.recovering = true;
                buffer.drain(..i);
                self.buffer = buffer;
                return Action::RequestSnapshot;
            }
            self.apply(msg);
        }
        buffer.clear();
        self.buffer = buffer; // keeps its allocation
        Action::Continue
    }

    fn apply(&mut self, msg: Msg) {
        if let Msg::Level { side, level, .. } = msg {
            let levels = match side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            if level.qty.0 == 0 {
                levels.remove(&level.price);
            } else {
                levels.insert(level.price, level);
            }
        }
        self.next += 1;
        self.at_boundary = msg.last();
        self.stats.applied += 1;
    }

    /// The sequence number of the last message in the book.
    pub fn seq(&self) -> u64 {
        self.next - 1
    }

    pub fn is_live(&self) -> bool {
        !self.recovering
    }

    /// Live and between commands: the book is one the engine actually had (D41).
    pub fn is_consistent(&self) -> bool {
        !self.recovering && self.at_boundary
    }

    /// Up to `n` levels on `side`, best first. L2 is `depth(side, n)`, L1 is `depth(side, 1)`.
    pub fn depth(&self, side: Side, n: usize) -> Vec<Level> {
        match side {
            Side::Buy => self.bids.values().rev().take(n).copied().collect(),
            Side::Sell => self.asks.values().take(n).copied().collect(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }
}

impl Default for Consumer {
    fn default() -> Self {
        Consumer::new()
    }
}

/// A seeded, unreliable link from a publisher to a consumer, for tests and `lob feed` (D44).
/// Every message and snapshot goes through its wire encoding (heartbeats don't; on a wire
/// one would be `seq | tag 4`). Messages are dropped and
/// duplicated at the given rates; a requested snapshot arrives 0–3 commands later, taken
/// either when it was requested or when it arrives.
pub struct Link {
    rng: Rng,
    drop_pct: u64,
    dup_pct: u64,
    /// Commands left before the requested snapshot arrives, and the snapshot if it was
    /// taken at request time.
    pending: Option<(u64, Option<Snapshot>)>,
    buf: Vec<u8>,
    /// Bytes delivered (duplicates count twice; dropped messages don't count).
    pub bytes: u64,
}

impl Link {
    pub fn new(seed: u64, drop_pct: u64, dup_pct: u64) -> Self {
        Link {
            rng: Rng::new(seed),
            drop_pct,
            dup_pct,
            pending: None,
            buf: Vec::new(),
            bytes: 0,
        }
    }

    /// Ask for a snapshot on the consumer's behalf, as its `RequestSnapshot` does.
    pub fn request_snapshot(&mut self, publisher: &Publisher) {
        if self.pending.is_none() {
            let early = self.rng.chance(50).then(|| publisher.snapshot());
            self.pending = Some((self.rng.below(4), early));
        }
    }

    /// Deliver one command's messages, then a heartbeat (also lossy). Call after `publisher`
    /// has seen the command.
    pub fn deliver(&mut self, publisher: &Publisher, msgs: &[Msg], consumer: &mut Consumer) {
        if let Some((wait, early)) = self.pending.take() {
            if wait == 0 {
                let snap = early.unwrap_or_else(|| publisher.snapshot());
                self.send_snapshot(&snap, publisher, consumer);
            } else {
                self.pending = Some((wait - 1, early));
            }
        }
        for msg in msgs {
            if self.rng.chance(self.drop_pct) {
                continue;
            }
            let copies = if self.rng.chance(self.dup_pct) { 2 } else { 1 };
            for _ in 0..copies {
                self.buf.clear();
                feed::encode(msg, &mut self.buf);
                self.bytes += self.buf.len() as u64;
                let (msg, _) = feed::decode(&self.buf).expect("a message we encoded");
                if consumer.on_msg(msg) == Action::RequestSnapshot {
                    self.request_snapshot(publisher);
                }
            }
        }
        if !self.rng.chance(self.drop_pct)
            && consumer.on_heartbeat(publisher.seq()) == Action::RequestSnapshot
        {
            self.request_snapshot(publisher);
        }
    }

    /// Deliver fresh snapshots, losslessly, until the consumer is live. At the end of a run.
    pub fn settle(&mut self, publisher: &Publisher, consumer: &mut Consumer) {
        self.pending = None;
        while !consumer.is_live() {
            self.send_snapshot(&publisher.snapshot(), publisher, consumer);
        }
    }

    fn send_snapshot(&mut self, snap: &Snapshot, publisher: &Publisher, consumer: &mut Consumer) {
        self.buf.clear();
        feed::encode_snapshot(snap, &mut self.buf);
        self.bytes += self.buf.len() as u64;
        let snap = feed::decode_snapshot(&self.buf).expect("a snapshot we encoded");
        if consumer.on_snapshot(&snap) == Action::RequestSnapshot {
            self.request_snapshot(publisher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Qty;

    fn bid(seq: u64, price: i64, qty: u64, last: bool) -> Msg {
        Msg::Level {
            seq,
            side: Side::Buy,
            level: Level {
                price: Price(price),
                qty: Qty(qty),
                orders: (qty > 0) as usize,
            },
            last,
        }
    }

    fn bids(c: &Consumer) -> Vec<(i64, u64)> {
        c.depth(Side::Buy, usize::MAX)
            .iter()
            .map(|l| (l.price.0, l.qty.0))
            .collect()
    }

    fn snapshot(seq: u64, levels: &[(i64, u64)]) -> Snapshot {
        Snapshot {
            seq,
            bids: levels
                .iter()
                .map(|&(p, q)| Level {
                    price: Price(p),
                    qty: Qty(q),
                    orders: 1,
                })
                .collect(),
            asks: Vec::new(),
        }
    }

    #[test]
    fn applies_in_order_and_ignores_duplicates() {
        let mut c = Consumer::new();
        assert_eq!(c.on_msg(bid(1, 99, 5, false)), Action::Continue);
        assert!(!c.is_consistent());
        c.on_msg(bid(2, 98, 3, true));
        c.on_msg(bid(1, 99, 7, true)); // a replay of 1 must not overwrite it
        c.on_msg(bid(3, 98, 0, true));
        assert_eq!(bids(&c), [(99, 5)]);
        assert!(c.is_consistent());
        assert_eq!((c.stats().applied, c.stats().duplicates), (3, 1));
        assert_eq!(c.depth(Side::Buy, 1)[0].price, Price(99));
    }

    #[test]
    fn a_gap_recovers_from_a_snapshot_and_the_buffer() {
        let mut c = Consumer::new();
        c.on_msg(bid(1, 99, 5, true));
        // 2 and 3 are lost.
        assert_eq!(c.on_msg(bid(4, 97, 1, true)), Action::RequestSnapshot);
        assert!(!c.is_live());
        c.on_msg(bid(5, 96, 1, true));
        c.on_msg(bid(5, 96, 1, true));
        // The snapshot covers up to 4, so 4 is dropped from the buffer and 5 applied.
        let snap = snapshot(4, &[(99, 5), (98, 2), (97, 1)]);
        assert_eq!(c.on_snapshot(&snap), Action::Continue);
        assert!(c.is_consistent());
        assert_eq!(bids(&c), [(99, 5), (98, 2), (97, 1), (96, 1)]);
        c.on_msg(bid(6, 95, 1, true));
        assert_eq!(bids(&c).len(), 5);
        let s = c.stats();
        assert_eq!((s.gaps, s.snapshots, s.duplicates), (1, 1, 1));
    }

    #[test]
    fn a_snapshot_older_than_the_buffer_asks_again() {
        let mut c = Consumer::new();
        assert_eq!(c.on_msg(bid(3, 97, 1, true)), Action::RequestSnapshot);
        c.on_msg(bid(5, 95, 1, true)); // 4 lost too
                                       // A snapshot as of 2 continues with 3 from the buffer, then finds 4 missing.
        assert_eq!(
            c.on_snapshot(&snapshot(2, &[(99, 5)])),
            Action::RequestSnapshot
        );
        assert!(!c.is_live());
        c.on_msg(bid(6, 94, 1, true));
        assert_eq!(c.on_snapshot(&snapshot(5, &[(95, 1)])), Action::Continue);
        assert_eq!(bids(&c), [(95, 1), (94, 1)]);
        assert_eq!((c.stats().gaps, c.stats().snapshots), (2, 2));
    }

    #[test]
    fn a_heartbeat_reveals_a_lost_last_message() {
        let mut c = Consumer::new();
        c.on_msg(bid(1, 99, 5, true));
        // 2 is lost and nothing follows: only the heartbeat says so.
        assert!(c.is_consistent());
        assert_eq!(c.on_heartbeat(1), Action::Continue);
        assert_eq!(c.on_heartbeat(2), Action::RequestSnapshot);
        assert!(!c.is_consistent());
        assert_eq!(c.on_heartbeat(2), Action::Continue); // already asked
        assert_eq!(c.on_snapshot(&snapshot(2, &[(98, 1)])), Action::Continue);
        assert_eq!((c.seq(), bids(&c)), (2, vec![(98, 1)]));
    }

    #[test]
    fn a_late_joiner_waits_for_its_snapshot() {
        let mut c = Consumer::late_joiner();
        assert!(!c.is_live());
        c.on_msg(bid(41, 99, 1, true));
        c.on_msg(bid(42, 98, 1, true));
        assert!(bids(&c).is_empty());
        assert_eq!(c.on_snapshot(&snapshot(41, &[(99, 1)])), Action::Continue);
        assert_eq!(bids(&c), [(99, 1), (98, 1)]);
        // A snapshot while live changes nothing.
        c.on_snapshot(&snapshot(0, &[]));
        assert_eq!(bids(&c).len(), 2);
    }
}
