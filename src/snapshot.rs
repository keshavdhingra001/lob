//! Book snapshots for crash recovery (D74–D76).
//!
//! A snapshot is the *logical* book, not either book's memory: the rules, the highest id
//! accepted, and every resting order in priority order. Any book can write one and any
//! book can restore it. Next to the book it records where the journal stood and the
//! replay counters, including the digest so far, so a recovered engine's event stream
//! continues byte for byte (D75).
//!
//! ```text
//! header:  "LOBS" | version u32
//!          tick_size i64 | max_qty u64 | has_last_id u8 | last_id u64
//!          commands u64 | events u64 | trades u64 | rejects u64 | digest u64
//!          journal_offset u64 | orders u64
//! order:   id u64 | side u8 | price i64 | qty u64 | post_only u8 | group u16 | action u8   (29 bytes)
//!          bids best first, then asks best first; oldest first within a price
//! trailer: crc32 u32 over everything before it
//! ```
//!
//! Every value has one spelling (no group is group 0 with action 0; no last id is flag 0
//! with id 0), so an accepted file re-encodes to the same bytes.

use std::collections::HashSet;
use std::num::NonZeroU16;

use thiserror::Error;

use crate::book::BookConfig;
use crate::command::Stp;
use crate::journal::{action_byte, byte_action, byte_side, side_byte};
use crate::replay::ReplayStats;
use crate::types::{OrderId, Price, Qty, Side};

pub const MAGIC: &[u8; 4] = b"LOBS";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 8 + 8 + 1 + 8 + 5 * 8 + 8 + 8;
const ORDER_LEN: usize = 29;
const TRAILER_LEN: usize = 4;

/// One resting order, as a snapshot records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub id: OrderId,
    pub side: Side,
    pub price: Price,
    /// Open quantity now, after any fills and in-place reductions.
    pub qty: Qty,
    pub post_only: bool,
    pub stp: Option<Stp>,
}

/// Everything a book's future behaviour depends on (D74).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookState {
    pub config: BookConfig,
    /// The highest id accepted (D30): a restored book must still reject reused ids.
    pub last_id: Option<OrderId>,
    /// Bids best first, then asks best first; oldest first within a price.
    pub orders: Vec<RestingOrder>,
}

impl BookState {
    /// Whether this is a book some session could have produced. `restore` relies on it:
    /// a book built from a bad state would break its invariants later, far from the cause.
    pub fn validate(&self) -> Result<(), &'static str> {
        let config = self.config;
        if config.tick_size <= 0 || config.max_qty == 0 {
            return Err("bad book config");
        }
        let mut ids = HashSet::with_capacity(self.orders.len());
        // Books keep per-level totals in a u64; the whole book fitting covers every level.
        let mut total = 0u64;
        let mut prev: Option<&RestingOrder> = None;
        let mut best_bid = None;
        for o in &self.orders {
            if o.qty.0 == 0 || o.qty.0 > config.max_qty {
                return Err("resting quantity out of range");
            }
            total = total
                .checked_add(o.qty.0)
                .ok_or("resting quantity overflows")?;
            if o.price.0 % config.tick_size != 0 {
                return Err("resting price off the tick grid");
            }
            if self.last_id.is_none_or(|last| o.id > last) {
                return Err("resting order id above the last id");
            }
            if !ids.insert(o.id) {
                return Err("duplicate resting order id");
            }
            match (prev.map(|p| (p.side, p.price)), o.side) {
                (Some((Side::Sell, _)), Side::Buy) => return Err("bid after the asks"),
                (Some((Side::Buy, p)), Side::Buy) if o.price > p => {
                    return Err("bids not best first")
                }
                (Some((Side::Sell, p)), Side::Sell) if o.price < p => {
                    return Err("asks not best first")
                }
                (Some((Side::Buy, _)) | None, Side::Sell) => {
                    if best_bid.is_some_and(|bid| bid >= o.price) {
                        return Err("crossed book");
                    }
                }
                (None, Side::Buy) => best_bid = Some(o.price),
                _ => {}
            }
            prev = Some(o);
        }
        Ok(())
    }
}

/// A book plus where the session stood when it was taken (D75).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub state: BookState,
    /// Counters and digest of the events produced by the first `stats.commands` journal
    /// records. `digest` is the running FNV state, so hashing can resume from it.
    pub stats: ReplayStats,
    /// The byte offset in the journal where record `stats.commands` (0-based) starts.
    pub journal_offset: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("not a snapshot (bad magic)")]
    BadMagic,
    #[error("unsupported snapshot version {0}")]
    UnsupportedVersion(u32),
    /// Wrong length or checksum: a snapshot is written whole or not at all (D76), so any
    /// damage means the file can't be trusted.
    #[error("damaged snapshot")]
    Damaged,
    #[error("invalid snapshot: {0}")]
    Invalid(&'static str),
}

impl Snapshot {
    pub fn encode(&self) -> Vec<u8> {
        let s = &self.state;
        let mut buf = Vec::with_capacity(HEADER_LEN + s.orders.len() * ORDER_LEN + TRAILER_LEN);
        let u64 = |buf: &mut Vec<u8>, v: u64| buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        u64(&mut buf, s.config.tick_size as u64);
        u64(&mut buf, s.config.max_qty);
        buf.push(s.last_id.is_some() as u8);
        u64(&mut buf, s.last_id.map_or(0, |id| id.0));
        let st = &self.stats;
        for v in [st.commands, st.events, st.trades, st.rejects, st.digest] {
            u64(&mut buf, v);
        }
        u64(&mut buf, self.journal_offset);
        u64(&mut buf, s.orders.len() as u64);
        for o in &s.orders {
            u64(&mut buf, o.id.0);
            buf.push(side_byte(o.side));
            u64(&mut buf, o.price.0 as u64);
            u64(&mut buf, o.qty.0);
            buf.push(o.post_only as u8);
            buf.extend_from_slice(&o.stp.map_or(0, |s| s.group.get()).to_le_bytes());
            buf.push(o.stp.map_or(0, |s| action_byte(s.action)));
        }
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decode and validate. Only a snapshot `encode` could have written is accepted.
    pub fn decode(bytes: &[u8]) -> Result<Snapshot, SnapshotError> {
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        if bytes.len() < HEADER_LEN + TRAILER_LEN {
            return Err(SnapshotError::Damaged);
        }
        let (body, trailer) = bytes.split_at(bytes.len() - TRAILER_LEN);
        if crc32fast::hash(body) != u32::from_le_bytes(trailer.try_into().unwrap()) {
            return Err(SnapshotError::Damaged);
        }
        let invalid = SnapshotError::Invalid;
        let mut r = Bytes(&body[8..]);
        let config = BookConfig {
            tick_size: r.u64() as i64,
            max_qty: r.u64(),
        };
        let last_id = match (r.u8(), r.u64()) {
            (0, 0) => None,
            (1, id) => Some(OrderId(id)),
            _ => return Err(invalid("bad last id")),
        };
        let stats = ReplayStats {
            commands: r.u64(),
            events: r.u64(),
            trades: r.u64(),
            rejects: r.u64(),
            digest: r.u64(),
        };
        let journal_offset = r.u64();
        let count = r.u64();
        // Checked before allocating, so a huge count in a small file can't ask for memory.
        if count != (r.0.len() / ORDER_LEN) as u64 || r.0.len() % ORDER_LEN != 0 {
            return Err(SnapshotError::Damaged);
        }
        let mut orders = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id = OrderId(r.u64());
            let side = byte_side(r.u8()).map_err(invalid)?;
            let price = Price(r.u64() as i64);
            let qty = Qty(r.u64());
            let post_only = match r.u8() {
                0 => false,
                1 => true,
                _ => return Err(invalid("bad post-only byte")),
            };
            let group = u16::from_le_bytes([r.u8(), r.u8()]);
            let stp = match (NonZeroU16::new(group), r.u8()) {
                (None, 0) => None,
                (Some(group), b) => Some(Stp {
                    group,
                    action: byte_action(b).map_err(invalid)?,
                }),
                (None, _) => return Err(invalid("stp action without a group")),
            };
            orders.push(RestingOrder {
                id,
                side,
                price,
                qty,
                post_only,
                stp,
            });
        }
        let state = BookState {
            config,
            last_id,
            orders,
        };
        state.validate().map_err(invalid)?;
        Ok(Snapshot {
            state,
            stats,
            journal_offset,
        })
    }
}

/// Reads fixed-width fields; the caller has checked the length.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn u8(&mut self) -> u8 {
        let b = self.0[0];
        self.0 = &self.0[1..];
        b
    }

    fn u64(&mut self) -> u64 {
        let (v, rest) = self.0.split_at(8);
        self.0 = rest;
        u64::from_le_bytes(v.try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::OrderBook;
    use crate::command::{Command, Event};
    use crate::{FastBook, RefBook};

    const SESSION: &[&str] = &[
        "limit 1 buy 10 100",
        "limit 2 buy 5 100 post",
        "limit 3 buy 7 99 gtc g=3 stp=co",
        "limit 4 sell 4 103",
        "limit 5 sell 6 102 post g=65535 stp=cb",
        "limit 6 sell 2 102",
        "modify 1 8 100",
        "market 7 sell 1",
        "limit 9 sell 3 -4",
        "cancel 9",
    ];

    fn parse(lines: &[&str]) -> Vec<Command> {
        lines.iter().map(|l| l.parse().unwrap()).collect()
    }

    fn book<B: OrderBook>(lines: &[&str]) -> B {
        let mut b = B::with_config(BookConfig {
            tick_size: 1,
            max_qty: 50,
        });
        let mut out = Vec::new();
        for cmd in parse(lines) {
            b.apply(&cmd, &mut out);
        }
        b
    }

    fn snapshot(state: BookState) -> Snapshot {
        Snapshot {
            state,
            stats: ReplayStats {
                commands: 10,
                events: 23,
                trades: 2,
                rejects: 1,
                digest: 0xdead_beef_0123_4567,
            },
            journal_offset: 1234,
        }
    }

    fn order(id: u64, side: Side, price: i64) -> RestingOrder {
        RestingOrder {
            id: OrderId(id),
            side,
            price: Price(price),
            qty: Qty(1),
            post_only: false,
            stp: None,
        }
    }

    #[test]
    fn both_books_write_the_same_state_in_priority_order() {
        let r = book::<RefBook>(SESSION).state();
        assert_eq!(r, book::<FastBook>(SESSION).state());
        let ids: Vec<u64> = r.orders.iter().map(|o| o.id.0).collect();
        // Bids 100 (1 then 2: 1 was reduced in place and kept its spot), 99; asks 102 (5, 6), 103.
        assert_eq!(ids, [1, 2, 3, 5, 6, 4]);
        assert_eq!(r.last_id, Some(OrderId(9)));
        assert!(r.orders[1].post_only);
        // Order 1 was reduced to 8 in place; the market sell took 1 and sell 9 (at -4) took 3.
        assert_eq!(
            (r.orders[0].qty, r.orders[3].stp.unwrap().group.get()),
            (Qty(4), 65535)
        );
    }

    #[test]
    fn a_restored_book_continues_like_the_original_in_both_directions() {
        let rest = parse(&[
            "limit 8 buy 20 102 g=65535 stp=cn",
            "limit 9 buy 1 90",
            "modify 2 9 101",
            "market 10 sell 30 g=3 stp=co",
            "limit 11 sell 1 100 post",
        ]);
        let run = |b: &mut dyn OrderBook| -> Vec<Event> {
            let mut out = Vec::new();
            for cmd in &rest {
                b.apply(cmd, &mut out);
                b.check_invariants().unwrap();
            }
            out
        };
        let state = book::<RefBook>(SESSION).state();
        let expected = run(&mut book::<RefBook>(SESSION));
        assert_eq!(run(&mut RefBook::from_state(&state).unwrap()), expected);
        assert_eq!(run(&mut FastBook::from_state(&state).unwrap()), expected);
        let fast_state = book::<FastBook>(SESSION).state();
        assert_eq!(
            run(&mut RefBook::from_state(&fast_state).unwrap()),
            expected
        );
        assert!(expected.contains(&Event::Rejected {
            id: OrderId(9),
            reason: crate::RejectReason::IdNotIncreasing
        }));
    }

    #[test]
    fn encoding_round_trips_and_has_the_documented_size() {
        let snap = snapshot(book::<FastBook>(SESSION).state());
        let bytes = snap.encode();
        assert_eq!(bytes.len(), 89 + 6 * 29 + 4);
        assert_eq!(Snapshot::decode(&bytes), Ok(snap));
        let empty = snapshot(RefBook::new().state());
        assert_eq!(Snapshot::decode(&empty.encode()), Ok(empty));
    }

    #[test]
    fn any_damage_is_refused() {
        let bytes = snapshot(book::<RefBook>(SESSION).state()).encode();
        for cut in 0..bytes.len() {
            assert!(Snapshot::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x10;
            assert!(Snapshot::decode(&b).is_err(), "flip at {i}");
        }
    }

    #[test]
    fn every_value_has_one_spelling() {
        // Edit a field, then re-seal the CRC so only the field check can refuse it.
        let edited = |at: usize, v: u8| {
            let mut b = snapshot(book::<RefBook>(SESSION).state()).encode();
            b[at] = v;
            let n = b.len() - TRAILER_LEN;
            let crc = crc32fast::hash(&b[..n]);
            b[n..].copy_from_slice(&crc.to_le_bytes());
            Snapshot::decode(&b)
        };
        // The first record (order 1) is ungrouped: give it an action byte.
        let first = HEADER_LEN;
        assert_eq!(
            edited(first + 28, 1),
            Err(SnapshotError::Invalid("stp action without a group"))
        );
        assert_eq!(
            edited(first + 25, 2),
            Err(SnapshotError::Invalid("bad post-only byte"))
        );
        // No last id with a nonzero id: flag at 24, id at 25.
        let mut b = snapshot(RefBook::new().state()).encode();
        b[25] = 1;
        let n = b.len() - TRAILER_LEN;
        let crc = crc32fast::hash(&b[..n]);
        b[n..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Snapshot::decode(&b),
            Err(SnapshotError::Invalid("bad last id"))
        );
    }

    #[test]
    fn validation_refuses_impossible_books() {
        let config = BookConfig {
            tick_size: 2,
            max_qty: 5,
        };
        let check = |last: u64, orders: Vec<RestingOrder>| {
            BookState {
                config,
                last_id: Some(OrderId(last)),
                orders,
            }
            .validate()
        };
        use Side::{Buy, Sell};
        assert_eq!(
            check(
                9,
                vec![
                    order(1, Buy, 4),
                    order(2, Buy, 2),
                    order(3, Sell, 6),
                    order(4, Sell, 6)
                ]
            ),
            Ok(())
        );
        assert_eq!(
            check(9, vec![order(1, Buy, 2), order(2, Buy, 4)]),
            Err("bids not best first")
        );
        assert_eq!(
            check(9, vec![order(1, Sell, 4), order(2, Sell, 2)]),
            Err("asks not best first")
        );
        assert_eq!(
            check(9, vec![order(1, Sell, 4), order(2, Buy, 2)]),
            Err("bid after the asks")
        );
        assert_eq!(
            check(9, vec![order(1, Buy, 4), order(2, Sell, 4)]),
            Err("crossed book")
        );
        assert_eq!(
            check(9, vec![order(1, Buy, 3)]),
            Err("resting price off the tick grid")
        );
        assert_eq!(
            check(9, vec![order(1, Buy, 2), order(1, Buy, 2)]),
            Err("duplicate resting order id")
        );
        assert_eq!(
            check(1, vec![order(2, Buy, 2)]),
            Err("resting order id above the last id")
        );
        let big = RestingOrder {
            qty: Qty(6),
            ..order(1, Buy, 2)
        };
        assert_eq!(check(9, vec![big]), Err("resting quantity out of range"));
        let empty = RestingOrder {
            qty: Qty(0),
            ..order(1, Buy, 2)
        };
        assert_eq!(check(9, vec![empty]), Err("resting quantity out of range"));
        let huge = BookState {
            config: BookConfig {
                tick_size: 1,
                max_qty: u64::MAX,
            },
            last_id: Some(OrderId(9)),
            orders: vec![
                RestingOrder {
                    qty: Qty(u64::MAX),
                    ..order(1, Buy, 2)
                },
                RestingOrder {
                    qty: Qty(1),
                    ..order(2, Buy, 2)
                },
            ],
        };
        assert_eq!(huge.validate(), Err("resting quantity overflows"));
        assert!(RefBook::from_state(&BookState {
            config,
            last_id: None,
            orders: vec![order(1, Buy, 2)]
        })
        .is_err());
    }
}
