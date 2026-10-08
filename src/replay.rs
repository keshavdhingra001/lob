//! Deterministic replay (D17): run a command sequence through a book and produce a
//! sequence-numbered binary event stream plus a digest of exactly those bytes.
//!
//! Same commands in, same bytes out, on any machine and any run. Two books that agree on
//! every event agree on the digest, so one 64-bit number compares whole sessions.
//!
//! Event record: `seq u64 | tag u8 | fields`, fixed-width little-endian:
//!
//! ```text
//! 1 accepted   id
//! 2 rejected   id | reason u8
//! 3 modified   id | qty | price
//! 4 trade      taker | maker | taker_side u8 | qty | price
//! 5 cancelled  id | remaining
//! 6 stp-cancelled  id | remaining
//! ```

use std::time::{Duration, Instant};

use crate::book::OrderBook;
use crate::command::{Command, Event, RejectReason};
use crate::types::Side;

/// FNV-1a, 64-bit. Hand-written and frozen: digests recorded today must still match
/// years from now. It isn't cryptographic; it detects accidental differences.
#[derive(Clone, Copy, Debug)]
pub struct Fnv64(u64);

impl Default for Fnv64 {
    fn default() -> Self {
        Fnv64(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv64 {
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// The hash so far. FNV's state *is* its output, so this is also where to resume.
    pub fn finish(self) -> u64 {
        self.0
    }

    /// Carry on hashing from an earlier `finish()` (D75).
    pub fn resume(state: u64) -> Self {
        Fnv64(state)
    }
}

/// Append one event record to `buf`.
pub fn encode_event(seq: u64, event: &Event, buf: &mut Vec<u8>) {
    let u64 = |buf: &mut Vec<u8>, v: u64| buf.extend_from_slice(&v.to_le_bytes());
    u64(buf, seq);
    match *event {
        Event::Accepted { id } => {
            buf.push(1);
            u64(buf, id.0);
        }
        Event::Rejected { id, reason } => {
            buf.push(2);
            u64(buf, id.0);
            buf.push(reason_byte(reason));
        }
        Event::Modified { id, qty, price } => {
            buf.push(3);
            u64(buf, id.0);
            u64(buf, qty.0);
            u64(buf, price.0 as u64);
        }
        Event::Trade {
            taker,
            maker,
            taker_side,
            qty,
            price,
        } => {
            buf.push(4);
            u64(buf, taker.0);
            u64(buf, maker.0);
            buf.push(match taker_side {
                Side::Buy => 0,
                Side::Sell => 1,
            });
            u64(buf, qty.0);
            u64(buf, price.0 as u64);
        }
        Event::Cancelled { id, remaining } => {
            buf.push(5);
            u64(buf, id.0);
            u64(buf, remaining.0);
        }
        Event::SelfTradeCancelled { id, remaining } => {
            buf.push(6);
            u64(buf, id.0);
            u64(buf, remaining.0);
        }
    }
}

fn reason_byte(reason: RejectReason) -> u8 {
    match reason {
        RejectReason::ZeroQty => 1,
        RejectReason::QtyTooLarge => 2,
        RejectReason::BadTick => 3,
        RejectReason::IdNotIncreasing => 4,
        RejectReason::UnknownOrder => 5,
        RejectReason::WouldCross => 6,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub commands: u64,
    pub events: u64,
    pub trades: u64,
    pub rejects: u64,
    /// FNV-1a 64 of the full encoded event stream.
    pub digest: u64,
}

/// The output side of a replay in progress: counters and digest of every event so far.
/// It can start from a snapshot's counters (D75), so a recovered session's digest equals
/// the uninterrupted one's. The book is passed in, so any book (or `&mut` one) works.
pub struct Recorder {
    stats: ReplayStats,
    hash: Fnv64,
    events: Vec<Event>,
    buf: Vec<u8>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            stats: ReplayStats::default(),
            hash: Fnv64::default(),
            events: Vec::with_capacity(64),
            buf: Vec::with_capacity(4096),
        }
    }
}

impl Recorder {
    /// Continue after `stats`: event numbers and the digest pick up where they stopped.
    pub fn resume(stats: ReplayStats) -> Self {
        Recorder {
            stats,
            hash: Fnv64::resume(stats.digest),
            ..Recorder::default()
        }
    }

    /// Apply one command to `book`. Encoded events go to `sink` in chunks.
    pub fn apply<B: OrderBook>(
        &mut self,
        book: &mut B,
        cmd: &Command,
        sink: &mut impl FnMut(&[u8]),
    ) {
        self.events.clear();
        book.apply(cmd, &mut self.events);
        for event in &self.events {
            self.stats.events += 1;
            match event {
                Event::Trade { .. } => self.stats.trades += 1,
                Event::Rejected { .. } => self.stats.rejects += 1,
                _ => {}
            }
            encode_event(self.stats.events, event, &mut self.buf);
        }
        self.stats.commands += 1;
        // Flush in chunks: one sink call per command would dominate the profile.
        if self.buf.len() >= 4000 {
            self.flush(sink);
        }
    }

    /// Hand any buffered event bytes to `sink`.
    pub fn flush(&mut self, sink: &mut impl FnMut(&[u8])) {
        self.hash.update(&self.buf);
        sink(&self.buf);
        self.buf.clear();
    }

    /// Counters and digest of every event so far, including any not yet flushed.
    pub fn stats(&self) -> ReplayStats {
        let mut hash = self.hash;
        hash.update(&self.buf);
        ReplayStats {
            digest: hash.finish(),
            ..self.stats
        }
    }
}

/// Run `commands` through `book`. Each chunk of encoded events is passed to `sink`
/// (to write it to a file, or ignore it); the digest covers exactly those bytes.
pub fn replay<B: OrderBook>(
    book: &mut B,
    commands: &[Command],
    mut sink: impl FnMut(&[u8]),
) -> ReplayStats {
    let mut rec = Recorder::default();
    for cmd in commands {
        rec.apply(book, cmd, &mut sink);
    }
    rec.flush(&mut sink);
    rec.stats()
}

/// `replay`, timed, without keeping the event bytes. For the CLI's throughput line;
/// M5 replaces this with per-command latency histograms.
pub fn replay_timed<B: OrderBook>(book: &mut B, commands: &[Command]) -> (ReplayStats, Duration) {
    let start = Instant::now();
    let stats = replay(book, commands, |_| {});
    (stats, start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::RefBook;
    use crate::types::{OrderId, Price, Qty};

    #[test]
    fn fnv_matches_published_test_vectors() {
        let fnv = |s: &str| {
            let mut h = Fnv64::default();
            h.update(s.as_bytes());
            h.finish()
        };
        assert_eq!(fnv(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn event_records_have_fixed_sizes_and_distinct_tags() {
        let id = OrderId(1);
        let sizes: Vec<(usize, u8)> = [
            Event::Accepted { id },
            Event::Rejected {
                id,
                reason: RejectReason::WouldCross,
            },
            Event::Modified {
                id,
                qty: Qty(1),
                price: Price(-1),
            },
            Event::Trade {
                taker: id,
                maker: OrderId(2),
                taker_side: Side::Sell,
                qty: Qty(1),
                price: Price(1),
            },
            Event::Cancelled {
                id,
                remaining: Qty(1),
            },
            Event::SelfTradeCancelled {
                id,
                remaining: Qty(1),
            },
        ]
        .iter()
        .map(|e| {
            let mut buf = Vec::new();
            encode_event(7, e, &mut buf);
            (buf.len(), buf[8])
        })
        .collect();
        assert_eq!(
            sizes,
            [(17, 1), (18, 2), (33, 3), (42, 4), (25, 5), (25, 6)],
            "(size, tag) per event kind: a shared tag would let the digest confuse two kinds"
        );
    }

    #[test]
    fn digest_covers_exactly_the_bytes_sent_to_the_sink() {
        let commands: Vec<Command> = [
            "limit 1 sell 10 100",
            "limit 2 buy 4 100",
            "cancel 9",
            "modify 1 2 100",
        ]
        .iter()
        .map(|l| l.parse().unwrap())
        .collect();
        let mut bytes = Vec::new();
        let stats = replay(&mut RefBook::new(), &commands, |b| {
            bytes.extend_from_slice(b)
        });
        let mut h = Fnv64::default();
        h.update(&bytes);
        assert_eq!(stats.digest, h.finish());
        assert_eq!(
            (stats.commands, stats.events, stats.trades, stats.rejects),
            (4, 5, 1, 1)
        );
        // Sequence numbers start at 1 and count every event.
        assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 1);
    }
}
