//! Market data out (M8): incremental level updates and trades with sequence numbers,
//! full-depth snapshots for recovery, and their binary wire format (D40–D43).
//!
//! ```text
//! level     seq u64 | tag 1 | flags u8 | price i64 | qty u64 | orders u32     30 bytes
//! trade     seq u64 | tag 2 | flags u8 | price i64 | qty u64                   26 bytes
//! snapshot  seq u64 | tag 3 | bids u32 | asks u32 | (price i64 | qty u64 | orders u32) per level
//! flags: bit 0 side (0 buy, 1 sell; the aggressor's for a trade), bit 1 last message of the command
//! ```

use std::collections::{BTreeMap, HashMap};

use crate::book::Level;
use crate::command::{Command, Event};
use crate::types::{OrderId, Price, Qty, Side};

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

    fn set_last(&mut self) {
        match self {
            Msg::Level { last, .. } | Msg::Trade { last, .. } => *last = true,
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

/// A resting order as the publisher sees it.
#[derive(Clone, Copy, Debug)]
struct Resting {
    side: Side,
    price: Price,
    open: u64,
}

/// The order the current command is working: a new order, or a modified one (D11 lets it
/// trade). It counts towards a level only if some of it is left at the end (D40).
#[derive(Clone, Copy, Debug)]
struct Taker {
    id: OrderId,
    side: Side,
    /// `None` for a market order, which never rests.
    price: Option<Price>,
    open: u64,
}

/// Builds the incremental feed from commands and their events, without looking inside
/// the book (D40). Its own book is kept by level, plus each resting order's place.
#[derive(Debug, Default)]
pub struct Publisher {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
    orders: HashMap<OrderId, Resting>,
    taker: Option<Taker>,
    /// Levels the current command changed, each with its state before the command.
    /// A `Vec` sorted at the end, not a map: the output order must not depend on hashing (D4).
    touched: Vec<(Side, Price, Level)>,
    seq: u64,
}

fn empty(price: Price) -> Level {
    Level {
        price,
        qty: Qty(0),
        orders: 0,
    }
}

impl Publisher {
    pub fn new() -> Self {
        Self::default()
    }

    /// The sequence number of the last message sent (0 before any).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Full depth now, as of the last message sent. Only valid between commands.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            seq: self.seq,
            bids: self.bids.values().rev().copied().collect(),
            asks: self.asks.values().copied().collect(),
        }
    }

    /// Append the messages for one command: its trades in event order, then every level
    /// whose total changed, bids then asks by price, the last one flagged (D41). An error
    /// means the events don't describe a valid book change, i.e. an engine bug.
    pub fn on_command(
        &mut self,
        cmd: &Command,
        events: &[Event],
        out: &mut Vec<Msg>,
    ) -> Result<(), String> {
        let start = out.len();
        for event in events {
            self.on_event(cmd, event, out)
                .map_err(|e| format!("after `{cmd}`, {event}: {e}"))?;
        }
        if let Some(t) = self.taker.take() {
            if t.open > 0 {
                let price = t.price.ok_or(format!("market order {} left open", t.id))?;
                self.rest(t.id, t.side, price, t.open)?;
            }
        }
        // Stable sort: for a level touched twice, the first entry holds its state before the command.
        self.touched
            .sort_by_key(|&(side, price, _)| (side == Side::Sell, price));
        self.touched
            .dedup_by_key(|&mut (side, price, _)| (side, price));
        for &(side, price, before) in &self.touched {
            let now = self
                .levels(side)
                .get(&price)
                .copied()
                .unwrap_or(empty(price));
            if now != before {
                self.seq += 1;
                out.push(Msg::Level {
                    seq: self.seq,
                    side,
                    level: now,
                    last: false,
                });
            }
        }
        self.touched.clear();
        if let Some(msg) = out[start..].last_mut() {
            msg.set_last();
        }
        Ok(())
    }

    fn on_event(&mut self, cmd: &Command, event: &Event, out: &mut Vec<Msg>) -> Result<(), String> {
        match *event {
            Event::Accepted { id } => {
                let (side, price, qty) = match *cmd {
                    Command::Limit {
                        id: c,
                        side,
                        price,
                        qty,
                        ..
                    } if c == id => (side, Some(price), qty),
                    Command::Market { id: c, side, qty } if c == id => (side, None, qty),
                    _ => return Err("doesn't match the command".into()),
                };
                self.start_taker(Taker {
                    id,
                    side,
                    price,
                    open: qty.0,
                })?;
            }
            Event::Rejected { .. } => {}
            Event::Modified { id, qty, price } => {
                let r = self.orders.remove(&id).ok_or("order isn't resting")?;
                self.reduce(r.side, r.price, r.open, true)?;
                self.start_taker(Taker {
                    id,
                    side: r.side,
                    price: Some(price),
                    open: qty.0,
                })?;
            }
            Event::Trade {
                taker,
                maker,
                taker_side,
                qty,
                price,
            } => {
                let t = self
                    .taker
                    .as_mut()
                    .filter(|t| t.id == taker && t.side == taker_side)
                    .ok_or("not the command's taker")?;
                t.open = t.open.checked_sub(qty.0).ok_or("taker overfilled")?;
                let m = self.orders.get_mut(&maker).ok_or("maker isn't resting")?;
                if m.price != price {
                    return Err("not at the maker's price".into());
                }
                m.open = m.open.checked_sub(qty.0).ok_or("maker overfilled")?;
                let (side, gone) = (m.side, m.open == 0);
                if gone {
                    self.orders.remove(&maker);
                }
                self.reduce(side, price, qty.0, gone)?;
                self.seq += 1;
                out.push(Msg::Trade {
                    seq: self.seq,
                    aggressor: taker_side,
                    price,
                    qty,
                    last: false,
                });
            }
            Event::Cancelled { id, remaining } => {
                let open = match self.taker {
                    Some(t) if t.id == id => {
                        self.taker = None;
                        t.open
                    }
                    _ => {
                        let r = self.orders.remove(&id).ok_or("order isn't live")?;
                        self.reduce(r.side, r.price, r.open, true)?;
                        r.open
                    }
                };
                if open != remaining.0 {
                    return Err(format!("{open} was open"));
                }
            }
        }
        Ok(())
    }

    fn start_taker(&mut self, taker: Taker) -> Result<(), String> {
        if self.taker.replace(taker).is_some() {
            return Err("a second taker in one command".into());
        }
        Ok(())
    }

    fn levels(&self, side: Side) -> &BTreeMap<Price, Level> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    /// The level at `price`, recording its state first if this command hasn't touched it yet.
    fn touch(&mut self, side: Side, price: Price) -> &mut Level {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.entry(price).or_insert(empty(price));
        self.touched.push((side, price, *level));
        level
    }

    fn rest(&mut self, id: OrderId, side: Side, price: Price, open: u64) -> Result<(), String> {
        if self
            .orders
            .insert(id, Resting { side, price, open })
            .is_some()
        {
            return Err(format!("order {id} rests twice"));
        }
        let level = self.touch(side, price);
        level.qty.0 += open;
        level.orders += 1;
        Ok(())
    }

    /// Take `qty` off a level, and the order too if it's `gone`. Drops the level when it empties.
    fn reduce(&mut self, side: Side, price: Price, qty: u64, gone: bool) -> Result<(), String> {
        let level = self.touch(side, price);
        level.qty.0 = level.qty.0.checked_sub(qty).ok_or("level overdrawn")?;
        level.orders = level
            .orders
            .checked_sub(gone as usize)
            .ok_or("level has no orders")?;
        if level.qty.0 == 0 {
            if level.orders != 0 {
                return Err("an empty level still has orders".into());
            }
            match side {
                Side::Buy => self.bids.remove(&price),
                Side::Sell => self.asks.remove(&price),
            };
        }
        Ok(())
    }
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
    use crate::book::OrderBook;
    use crate::reference::RefBook;

    fn level(price: i64, qty: u64, orders: usize) -> Level {
        Level {
            price: Price(price),
            qty: Qty(qty),
            orders,
        }
    }

    /// Run `lines` through a reference book and return each command's messages.
    fn feed(lines: &[&str]) -> Vec<Vec<Msg>> {
        let mut book = RefBook::new();
        let mut publisher = Publisher::new();
        lines
            .iter()
            .map(|line| {
                let cmd: Command = line.parse().unwrap();
                let mut events = Vec::new();
                book.apply(&cmd, &mut events);
                let mut out = Vec::new();
                publisher.on_command(&cmd, &events, &mut out).unwrap();
                out
            })
            .collect()
    }

    fn lvl(seq: u64, side: Side, price: i64, qty: u64, orders: usize, last: bool) -> Msg {
        Msg::Level {
            seq,
            side,
            level: level(price, qty, orders),
            last,
        }
    }

    #[test]
    fn a_sweep_sends_its_trades_then_each_level_once() {
        let out = feed(&[
            "limit 1 sell 10 100",
            "limit 2 sell 10 100",
            "limit 3 sell 5 101",
            "limit 4 buy 22 101",
        ]);
        assert_eq!(out[0], [lvl(1, Side::Sell, 100, 10, 1, true)]);
        assert_eq!(out[1], [lvl(2, Side::Sell, 100, 20, 2, true)]);
        let trade = |seq, price, qty| Msg::Trade {
            seq,
            aggressor: Side::Buy,
            price: Price(price),
            qty: Qty(qty),
            last: false,
        };
        // 22 takes both orders at 100 and 2 of the 5 at 101. Nothing rests.
        assert_eq!(
            out[3],
            [
                trade(4, 100, 10),
                trade(5, 100, 10),
                trade(6, 101, 2),
                lvl(7, Side::Sell, 100, 0, 0, false),
                lvl(8, Side::Sell, 101, 3, 1, true),
            ]
        );
    }

    #[test]
    fn a_crossing_modify_shows_only_the_final_book() {
        let out = feed(&["limit 1 sell 5 101", "limit 2 buy 8 99", "modify 2 8 101"]);
        // The order never visibly rests at 101 before its trade: only the result is sent.
        assert_eq!(
            out[2],
            [
                Msg::Trade {
                    seq: 3,
                    aggressor: Side::Buy,
                    price: Price(101),
                    qty: Qty(5),
                    last: false,
                },
                lvl(4, Side::Buy, 99, 0, 0, false),
                lvl(5, Side::Buy, 101, 3, 1, false),
                lvl(6, Side::Sell, 101, 0, 0, true),
            ]
        );
    }

    #[test]
    fn commands_that_change_no_level_send_nothing() {
        let out = feed(&[
            "limit 1 buy 5 99",
            "modify 1 5 99",          // same quantity and price
            "cancel 7",               // unknown
            "limit 2 sell 9 100 ioc", // nothing to trade with
            "limit 3 sell 9 99 fok",  // can't fill all 9
            "limit 4 buy 1 101 post",
            "limit 5 sell 1 99 post", // would cross
        ]);
        let sizes: Vec<usize> = out.iter().map(Vec::len).collect();
        assert_eq!(sizes, [1, 0, 0, 0, 0, 1, 0]);
    }

    #[test]
    fn partial_fills_and_cancels_update_a_level() {
        let out = feed(&[
            "limit 1 buy 5 99",
            "limit 2 buy 7 99",
            "market 3 sell 3",
            "modify 2 4 99",
            "cancel 1",
        ]);
        assert_eq!(out[2][1], lvl(4, Side::Buy, 99, 9, 2, true));
        assert_eq!(out[3], [lvl(5, Side::Buy, 99, 6, 2, true)]);
        assert_eq!(out[4], [lvl(6, Side::Buy, 99, 4, 1, true)]);
    }

    #[test]
    fn inconsistent_events_are_errors() {
        let cmd: Command = "limit 1 buy 5 99".parse().unwrap();
        let mut p = Publisher::new();
        let mut out = Vec::new();
        let unknown = Event::Cancelled {
            id: OrderId(9),
            remaining: Qty(1),
        };
        assert!(p.on_command(&cmd, &[unknown], &mut out).is_err());

        let mut p = Publisher::new();
        let accepted = Event::Accepted { id: OrderId(1) };
        p.on_command(&cmd, &[accepted], &mut out).unwrap();
        let cancel = Command::Cancel { id: OrderId(1) };
        let wrong = Event::Cancelled {
            id: OrderId(1),
            remaining: Qty(4),
        };
        assert!(p.on_command(&cancel, &[wrong], &mut out).is_err());
        let market: Command = "market 2 sell 1".parse().unwrap();
        let accepted = Event::Accepted { id: OrderId(2) };
        assert_eq!(
            p.on_command(&market, &[accepted], &mut out),
            Err("market order 2 left open".into())
        );
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
