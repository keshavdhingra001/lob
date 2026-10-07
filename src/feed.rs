//! Market data out (M8): incremental level updates and trades with sequence numbers,
//! full-depth snapshots for recovery, and their binary wire format (D40–D43).
//!
//! ```text
//! level     seq u64 | tag 1 | flags u8 | price i64 | qty u64 | orders u32     30 bytes
//! trade     seq u64 | tag 2 | flags u8 | price i64 | qty u64                   26 bytes
//! snapshot  seq u64 | tag 3 | bids u32 | asks u32 | (price i64 | qty u64 | orders u32) per level
//! flags: bit 0 side (0 buy, 1 sell; the aggressor's for a trade), bit 1 last message of the command
//! ```

use crate::book::Level;
use crate::types::{Price, Qty, Side};

/// One incremental market data message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    /// A price level's new total. `qty` 0 means the level is gone (D41).
    Level {
        seq: u64,
        side: Side,
        level: Level,
        last: bool,
    },
    /// One fill, at the maker's price. Anonymous: no order ids.
    Trade {
        seq: u64,
        aggressor: Side,
        price: Price,
        qty: Qty,
        last: bool,
    },
}

impl Msg {
    pub fn seq(&self) -> u64 {
        match *self {
            Msg::Level { seq, .. } | Msg::Trade { seq, .. } => seq,
        }
    }

    /// Whether this is the last message of one command's batch (D41).
    pub fn last(&self) -> bool {
        match *self {
            Msg::Level { last, .. } | Msg::Trade { last, .. } => last,
        }
    }
}

/// Both sides' full depth, best first, as of message `seq` (0: before any message).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub seq: u64,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

pub const LEVEL_LEN: usize = 30;
pub const TRADE_LEN: usize = 26;
const LEVEL_TAG: u8 = 1;
const TRADE_TAG: u8 = 2;
const SNAPSHOT_TAG: u8 = 3;
const SELL: u8 = 1;
const LAST: u8 = 2;
const SNAPSHOT_LEVEL_LEN: usize = 20;

fn flags(side: Side, last: bool) -> u8 {
    let side = match side {
        Side::Buy => 0,
        Side::Sell => SELL,
    };
    side | if last { LAST } else { 0 }
}

/// Append one message to `buf`.
pub fn encode(msg: &Msg, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&msg.seq().to_le_bytes());
    match *msg {
        Msg::Level {
            side, level, last, ..
        } => {
            buf.push(LEVEL_TAG);
            buf.push(flags(side, last));
            buf.extend_from_slice(&level.price.0.to_le_bytes());
            buf.extend_from_slice(&level.qty.0.to_le_bytes());
            buf.extend_from_slice(&level_orders(&level).to_le_bytes());
        }
        Msg::Trade {
            aggressor,
            price,
            qty,
            last,
            ..
        } => {
            buf.push(TRADE_TAG);
            buf.push(flags(aggressor, last));
            buf.extend_from_slice(&price.0.to_le_bytes());
            buf.extend_from_slice(&qty.0.to_le_bytes());
        }
    }
}

/// Order counts go out as `u32`; four billion orders at one price can't happen in a
/// book whose ids and memory are bounded long before that.
fn level_orders(level: &Level) -> u32 {
    u32::try_from(level.orders).expect("order count fits in u32")
}

/// Little-endian reader over a byte slice that fails on running out instead of panicking.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        if self.0.len() < N {
            return Err("truncated message");
        }
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        Ok(head.try_into().unwrap())
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn i64(&mut self) -> Result<i64, &'static str> {
        Ok(i64::from_le_bytes(self.take()?))
    }

    fn level(&mut self) -> Result<Level, &'static str> {
        Ok(Level {
            price: Price(self.i64()?),
            qty: Qty(self.u64()?),
            orders: self.u32()? as usize,
        })
    }
}

/// Decode the message at the start of `bytes`; returns it and its length.
pub fn decode(bytes: &[u8]) -> Result<(Msg, usize), &'static str> {
    let mut c = Cursor(bytes);
    let seq = c.u64()?;
    let tag = c.u8()?;
    let flags = c.u8()?;
    if flags & !(SELL | LAST) != 0 {
        return Err("unknown flag bits");
    }
    let side = if flags & SELL == 0 {
        Side::Buy
    } else {
        Side::Sell
    };
    let last = flags & LAST != 0;
    let msg = match tag {
        LEVEL_TAG => Msg::Level {
            seq,
            side,
            level: c.level()?,
            last,
        },
        TRADE_TAG => Msg::Trade {
            seq,
            aggressor: side,
            price: Price(c.i64()?),
            qty: Qty(c.u64()?),
            last,
        },
        _ => return Err("unknown message tag"),
    };
    Ok((msg, bytes.len() - c.0.len()))
}

/// Append a snapshot to `buf`.
pub fn encode_snapshot(snap: &Snapshot, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&snap.seq.to_le_bytes());
    buf.push(SNAPSHOT_TAG);
    for side in [&snap.bids, &snap.asks] {
        let n = u32::try_from(side.len()).expect("level count fits in u32");
        buf.extend_from_slice(&n.to_le_bytes());
    }
    for level in snap.bids.iter().chain(&snap.asks) {
        buf.extend_from_slice(&level.price.0.to_le_bytes());
        buf.extend_from_slice(&level.qty.0.to_le_bytes());
        buf.extend_from_slice(&level_orders(level).to_le_bytes());
    }
}

/// Decode a snapshot that fills `bytes` exactly.
pub fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot, &'static str> {
    let mut c = Cursor(bytes);
    let seq = c.u64()?;
    if c.u8()? != SNAPSHOT_TAG {
        return Err("not a snapshot");
    }
    let (bids, asks) = (c.u32()? as usize, c.u32()? as usize);
    // Check the length before allocating, so a corrupt count can't ask for gigabytes.
    if (bids + asks).checked_mul(SNAPSHOT_LEVEL_LEN) != Some(c.0.len()) {
        return Err("snapshot length doesn't match its level counts");
    }
    let mut side = |n| (0..n).map(|_| c.level()).collect::<Result<Vec<_>, _>>();
    let bids = side(bids)?;
    let asks = side(asks)?;
    Ok(Snapshot { seq, bids, asks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: i64, qty: u64, orders: usize) -> Level {
        Level {
            price: Price(price),
            qty: Qty(qty),
            orders,
        }
    }

    #[test]
    fn a_level_update_byte_by_byte() {
        let msg = Msg::Level {
            seq: 0x0102,
            side: Side::Sell,
            level: level(-2, 3, 4),
            last: true,
        };
        let mut buf = Vec::new();
        encode(&msg, &mut buf);
        let mut want = vec![0x02, 0x01, 0, 0, 0, 0, 0, 0, LEVEL_TAG, SELL | LAST];
        want.extend_from_slice(&[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        want.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0]);
        assert_eq!(buf, want);
        assert_eq!(decode(&buf), Ok((msg, LEVEL_LEN)));
    }

    #[test]
    fn messages_round_trip_with_extreme_values() {
        let msgs = [
            Msg::Level {
                seq: u64::MAX,
                side: Side::Buy,
                level: level(i64::MIN, u64::MAX, u32::MAX as usize),
                last: false,
            },
            Msg::Level {
                seq: 1,
                side: Side::Sell,
                level: level(i64::MAX, 0, 0),
                last: true,
            },
            Msg::Trade {
                seq: 7,
                aggressor: Side::Sell,
                price: Price(-1),
                qty: Qty(u64::MAX),
                last: false,
            },
            Msg::Trade {
                seq: 8,
                aggressor: Side::Buy,
                price: Price(100),
                qty: Qty(1),
                last: true,
            },
        ];
        let mut buf = Vec::new();
        for msg in &msgs {
            encode(msg, &mut buf);
        }
        let mut at = 0;
        for msg in &msgs {
            let (got, len) = decode(&buf[at..]).unwrap();
            assert_eq!(got, *msg);
            let want = match msg {
                Msg::Level { .. } => LEVEL_LEN,
                Msg::Trade { .. } => TRADE_LEN,
            };
            assert_eq!(len, want);
            at += len;
        }
        assert_eq!(at, buf.len());
    }

    #[test]
    fn bad_messages_are_refused() {
        let mut buf = Vec::new();
        encode(
            &Msg::Trade {
                seq: 1,
                aggressor: Side::Buy,
                price: Price(1),
                qty: Qty(1),
                last: false,
            },
            &mut buf,
        );
        for cut in 0..buf.len() {
            assert_eq!(
                decode(&buf[..cut]),
                Err("truncated message"),
                "cut at {cut}"
            );
        }
        let mut bad = buf.clone();
        bad[8] = 9;
        assert_eq!(decode(&bad), Err("unknown message tag"));
        bad = buf.clone();
        bad[9] = 4;
        assert_eq!(decode(&bad), Err("unknown flag bits"));
    }

    #[test]
    fn snapshots_round_trip_and_check_their_length() {
        let snap = Snapshot {
            seq: 42,
            bids: vec![level(100, 5, 1), level(99, 7, 2)],
            asks: vec![level(101, 3, 1)],
        };
        let mut buf = Vec::new();
        encode_snapshot(&snap, &mut buf);
        assert_eq!(buf.len(), 17 + 3 * SNAPSHOT_LEVEL_LEN);
        assert_eq!(decode_snapshot(&buf), Ok(snap));

        let empty = Snapshot::default();
        let mut buf0 = Vec::new();
        encode_snapshot(&empty, &mut buf0);
        assert_eq!(decode_snapshot(&buf0), Ok(empty));

        assert!(decode_snapshot(&buf[..buf.len() - 1]).is_err());
        let mut huge = buf.clone();
        huge[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_snapshot(&huge),
            Err("snapshot length doesn't match its level counts")
        );
        huge = buf.clone();
        huge[8] = LEVEL_TAG;
        assert_eq!(decode_snapshot(&huge), Err("not a snapshot"));
    }
}
