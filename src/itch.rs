//! NASDAQ TotalView-ITCH 5.0 (D36): framing and decoding, with no allocation per message.
//!
//! A sample file is a stream of messages, each behind a 2-byte big-endian length (NASDAQ's
//! "BinaryFILE" framing). Every message starts with the same 11-byte header:
//!
//! ```text
//! type u8 | stock locate u16 | tracking number u16 | timestamp u48 (ns since midnight)
//! ```
//!
//! and then fixed-width big-endian fields that depend on the type. Every type has one
//! fixed length, so a length that disagrees with the type is a hard error: it means the
//! framing slipped, and everything after it would be garbage.
//!
//! [`Reader`] hands out each message as a slice of its own buffer, and [`decode`] reads
//! the fields straight out of that slice into a small `Copy` value. Nothing is allocated
//! after the reader's one buffer, however long the file is. All 23 message types are
//! framed and length-checked; the ones the book and the checks need are decoded, and the
//! rest come back as [`Body::Other`].
//!
//! Prices are `u32` in 1/10,000 dollars (four implied decimals), as on the wire.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use thiserror::Error;

use crate::types::Side;

/// Bytes before the type-specific fields: type, locate, tracking number, timestamp.
pub const HEADER_LEN: usize = 11;

/// Reader buffer. Any message (at most 50 bytes in 5.0) fits many times over.
const BUF_LEN: usize = 1 << 20;

/// The fixed length of each 5.0 message type, including the type byte.
pub fn message_len(kind: u8) -> Option<usize> {
    Some(match kind {
        b'S' => 12, // system event
        b'R' => 39, // stock directory
        b'H' => 25, // stock trading action
        b'Y' => 20, // Reg SHO restriction
        b'L' => 26, // market participant position
        b'V' => 35, // MWCB decline level
        b'W' => 12, // MWCB status
        b'K' => 28, // IPO quoting period update
        b'J' => 35, // LULD auction collar
        b'h' => 21, // operational halt
        b'A' => 36, // add order
        b'F' => 40, // add order with MPID attribution
        b'E' => 31, // order executed
        b'C' => 36, // order executed with price
        b'X' => 23, // order cancel (partial)
        b'D' => 19, // order delete
        b'U' => 35, // order replace
        b'P' => 44, // trade (non-cross)
        b'Q' => 40, // cross trade
        b'B' => 19, // broken trade
        b'I' => 50, // net order imbalance indicator
        b'N' => 20, // retail price improvement indicator
        b'O' => 48, // direct listing with capital raise
        _ => return None,
    })
}

/// An 8-byte, space-padded stock symbol, as on the wire.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stock(pub [u8; 8]);

impl Stock {
    /// Pads `name` with spaces. Panics if it's longer than 8 bytes (test helper).
    pub fn new(name: &str) -> Stock {
        assert!(name.len() <= 8, "symbol `{name}` is longer than 8 bytes");
        let mut b = [b' '; 8];
        b[..name.len()].copy_from_slice(name.as_bytes());
        Stock(b)
    }
}

impl fmt::Display for Stock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = String::from_utf8_lossy(&self.0);
        f.write_str(s.trim_end())
    }
}

impl fmt::Debug for Stock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stock({self})")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// NASDAQ's index for the symbol, assigned by the day's stock directory messages.
    /// 0 for messages that aren't about one symbol (system events).
    pub locate: u16,
    pub tracking: u16,
    /// Nanoseconds since midnight (Eastern).
    pub timestamp: u64,
}

/// The decoded fields of one message. Only the types the book and its checks use are
/// decoded; the rest are [`Body::Other`] with their type byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
    /// `S`: O start of messages, S start of system hours, Q start of market hours,
    /// M end of market hours, E end of system hours, C end of messages.
    SystemEvent { code: u8 },
    /// `R`: binds `header.locate` to a symbol for the day.
    StockDirectory { stock: Stock },
    /// `H`: T trading, H halted, P paused, Q quotation only.
    TradingAction { stock: Stock, state: u8 },
    /// `A` and `F` (`F` adds a market participant id, dropped here).
    AddOrder {
        order_ref: u64,
        side: Side,
        shares: u32,
        stock: Stock,
        price: u32,
    },
    /// `E`: a resting order traded at its own price.
    Executed {
        order_ref: u64,
        shares: u32,
        match_number: u64,
    },
    /// `C`: a resting order traded at a price that may differ from its own (in a cross).
    ExecutedWithPrice {
        order_ref: u64,
        shares: u32,
        match_number: u64,
        printable: bool,
        price: u32,
    },
    /// `X`: part of an order cancelled; the rest stays.
    Cancel { order_ref: u64, shares: u32 },
    /// `D`: the whole remaining order removed.
    Delete { order_ref: u64 },
    /// `U`: cancel `old_ref` and add `new_ref` with the same side and symbol, new
    /// shares and price, and new time priority.
    Replace {
        old_ref: u64,
        new_ref: u64,
        shares: u32,
        price: u32,
    },
    /// `P`: an execution against a non-displayed order. It never touches the visible book.
    Trade {
        side: Side,
        shares: u32,
        stock: Stock,
        price: u32,
        match_number: u64,
    },
    /// `Q`: an auction (cross) print. `cross_type`: O opening, C closing, H halt/IPO, I intraday.
    CrossTrade {
        shares: u64,
        stock: Stock,
        price: u32,
        match_number: u64,
        cross_type: u8,
    },
    /// Framed and length-checked, not decoded.
    Other(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Body,
}

/// A message that's the wrong shape for its type.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("empty message")]
    Empty,
    #[error("unknown message type {0:#04x}")]
    UnknownType(u8),
    #[error("type `{}` should be {expected} bytes, got {got}", *kind as char)]
    BadLength {
        kind: u8,
        expected: usize,
        got: usize,
    },
    #[error("type `{}`: invalid side byte {side:#04x}", *kind as char)]
    BadSide { kind: u8, side: u8 },
}

#[derive(Debug, Error)]
pub enum ItchError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("message {index} at byte {offset}: {source}")]
    Decode {
        index: u64,
        offset: u64,
        source: DecodeError,
    },
    #[error("file ends inside a message: {0} bytes after the last whole one")]
    Truncated(usize),
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().expect("4 bytes"))
}

fn u48_at(b: &[u8], i: usize) -> u64 {
    let mut x = [0u8; 8];
    x[2..].copy_from_slice(&b[i..i + 6]);
    u64::from_be_bytes(x)
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().expect("8 bytes"))
}

fn stock_at(b: &[u8], i: usize) -> Stock {
    Stock(b[i..i + 8].try_into().expect("8 bytes"))
}

fn side_at(b: &[u8], i: usize) -> Result<Side, DecodeError> {
    match b[i] {
        b'B' => Ok(Side::Buy),
        b'S' => Ok(Side::Sell),
        side => Err(DecodeError::BadSide { kind: b[0], side }),
    }
}

/// Decode one message (without its 2-byte length prefix).
pub fn decode(b: &[u8]) -> Result<Message, DecodeError> {
    let kind = *b.first().ok_or(DecodeError::Empty)?;
    let expected = message_len(kind).ok_or(DecodeError::UnknownType(kind))?;
    if b.len() != expected {
        return Err(DecodeError::BadLength {
            kind,
            expected,
            got: b.len(),
        });
    }
    let header = Header {
        locate: u16_at(b, 1),
        tracking: u16_at(b, 3),
        timestamp: u48_at(b, 5),
    };
    // Field offsets below are from the 5.0 spec's tables; 11 is the first byte after the header.
    let body = match kind {
        b'S' => Body::SystemEvent { code: b[11] },
        b'R' => Body::StockDirectory {
            stock: stock_at(b, 11),
        },
        b'H' => Body::TradingAction {
            stock: stock_at(b, 11),
            state: b[19],
        },
        b'A' | b'F' => Body::AddOrder {
            order_ref: u64_at(b, 11),
            side: side_at(b, 19)?,
            shares: u32_at(b, 20),
            stock: stock_at(b, 24),
            price: u32_at(b, 32),
        },
        b'E' => Body::Executed {
            order_ref: u64_at(b, 11),
            shares: u32_at(b, 19),
            match_number: u64_at(b, 23),
        },
        b'C' => Body::ExecutedWithPrice {
            order_ref: u64_at(b, 11),
            shares: u32_at(b, 19),
            match_number: u64_at(b, 23),
            printable: b[31] == b'Y',
            price: u32_at(b, 32),
        },
        b'X' => Body::Cancel {
            order_ref: u64_at(b, 11),
            shares: u32_at(b, 19),
        },
        b'D' => Body::Delete {
            order_ref: u64_at(b, 11),
        },
        b'U' => Body::Replace {
            old_ref: u64_at(b, 11),
            new_ref: u64_at(b, 19),
            shares: u32_at(b, 27),
            price: u32_at(b, 31),
        },
        b'P' => Body::Trade {
            side: side_at(b, 19)?,
            shares: u32_at(b, 20),
            stock: stock_at(b, 24),
            price: u32_at(b, 32),
            match_number: u64_at(b, 36),
        },
        b'Q' => Body::CrossTrade {
            shares: u64_at(b, 11),
            stock: stock_at(b, 19),
            price: u32_at(b, 27),
            match_number: u64_at(b, 31),
            cross_type: b[39],
        },
        other => Body::Other(other),
    };
    Ok(Message { header, body })
}

/// Encode a decoded message, with its 2-byte length prefix, for tests and synthetic
/// files. `Other` and the dropped fields (`F`'s attribution, `R`'s and `H`'s extra
/// fields) can't be rebuilt, so `Other` panics and the rest are written as zeros
/// (`AddOrder` is always written as `A`).
pub fn encode(msg: &Message, out: &mut Vec<u8>) {
    let kind = match msg.body {
        Body::SystemEvent { .. } => b'S',
        Body::StockDirectory { .. } => b'R',
        Body::TradingAction { .. } => b'H',
        Body::AddOrder { .. } => b'A',
        Body::Executed { .. } => b'E',
        Body::ExecutedWithPrice { .. } => b'C',
        Body::Cancel { .. } => b'X',
        Body::Delete { .. } => b'D',
        Body::Replace { .. } => b'U',
        Body::Trade { .. } => b'P',
        Body::CrossTrade { .. } => b'Q',
        Body::Other(k) => panic!("can't encode undecoded type {}", k as char),
    };
    let len = message_len(kind).expect("known type");
    let mut b = [0u8; 64];
    b[0] = kind;
    b[1..3].copy_from_slice(&msg.header.locate.to_be_bytes());
    b[3..5].copy_from_slice(&msg.header.tracking.to_be_bytes());
    b[5..11].copy_from_slice(&msg.header.timestamp.to_be_bytes()[2..]);
    let side = |s: Side| if s == Side::Buy { b'B' } else { b'S' };
    match msg.body {
        Body::SystemEvent { code } => b[11] = code,
        Body::StockDirectory { stock } => b[11..19].copy_from_slice(&stock.0),
        Body::TradingAction { stock, state } => {
            b[11..19].copy_from_slice(&stock.0);
            b[19] = state;
        }
        Body::AddOrder {
            order_ref,
            side: s,
            shares,
            stock,
            price,
        } => {
            b[11..19].copy_from_slice(&order_ref.to_be_bytes());
            b[19] = side(s);
            b[20..24].copy_from_slice(&shares.to_be_bytes());
            b[24..32].copy_from_slice(&stock.0);
            b[32..36].copy_from_slice(&price.to_be_bytes());
        }
        Body::Executed {
            order_ref,
            shares,
            match_number,
        } => {
            b[11..19].copy_from_slice(&order_ref.to_be_bytes());
            b[19..23].copy_from_slice(&shares.to_be_bytes());
            b[23..31].copy_from_slice(&match_number.to_be_bytes());
        }
        Body::ExecutedWithPrice {
            order_ref,
            shares,
            match_number,
            printable,
            price,
        } => {
            b[11..19].copy_from_slice(&order_ref.to_be_bytes());
            b[19..23].copy_from_slice(&shares.to_be_bytes());
            b[23..31].copy_from_slice(&match_number.to_be_bytes());
            b[31] = if printable { b'Y' } else { b'N' };
            b[32..36].copy_from_slice(&price.to_be_bytes());
        }
        Body::Cancel { order_ref, shares } => {
            b[11..19].copy_from_slice(&order_ref.to_be_bytes());
            b[19..23].copy_from_slice(&shares.to_be_bytes());
        }
        Body::Delete { order_ref } => b[11..19].copy_from_slice(&order_ref.to_be_bytes()),
        Body::Replace {
            old_ref,
            new_ref,
            shares,
            price,
        } => {
            b[11..19].copy_from_slice(&old_ref.to_be_bytes());
            b[19..27].copy_from_slice(&new_ref.to_be_bytes());
            b[27..31].copy_from_slice(&shares.to_be_bytes());
            b[31..35].copy_from_slice(&price.to_be_bytes());
        }
        Body::Trade {
            side: s,
            shares,
            stock,
            price,
            match_number,
        } => {
            b[19] = side(s);
            b[20..24].copy_from_slice(&shares.to_be_bytes());
            b[24..32].copy_from_slice(&stock.0);
            b[32..36].copy_from_slice(&price.to_be_bytes());
            b[36..44].copy_from_slice(&match_number.to_be_bytes());
        }
        Body::CrossTrade {
            shares,
            stock,
            price,
            match_number,
            cross_type,
        } => {
            b[11..19].copy_from_slice(&shares.to_be_bytes());
            b[19..27].copy_from_slice(&stock.0);
            b[27..31].copy_from_slice(&price.to_be_bytes());
            b[31..39].copy_from_slice(&match_number.to_be_bytes());
            b[39] = cross_type;
        }
        Body::Other(_) => unreachable!(),
    }
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&b[..len]);
}

/// Splits a byte stream into messages. Each one is a slice of the reader's buffer, valid
/// until the next call (so this is a method rather than an `Iterator`).
pub struct Reader<R> {
    inner: R,
    buf: Box<[u8]>,
    /// Unconsumed bytes are `buf[start..end]`.
    start: usize,
    end: usize,
    /// Stream offset of `buf[start]`, for error messages.
    offset: u64,
    index: u64,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Self {
        Reader {
            inner,
            buf: vec![0; BUF_LEN].into_boxed_slice(),
            start: 0,
            end: 0,
            offset: 0,
            index: 0,
        }
    }

    /// Messages returned so far.
    pub fn count(&self) -> u64 {
        self.index
    }

    /// Bytes consumed so far, length prefixes included.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Make at least `need` unconsumed bytes available. Returns false at a clean end
    /// of stream (nothing left at all); a partial message at the end is `Truncated`.
    fn fill(&mut self, need: usize) -> Result<bool, ItchError> {
        while self.end - self.start < need {
            if self.end == self.buf.len() {
                // Out of room at the back: slide the unconsumed tail to the front.
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            let n = match self.inner.read(&mut self.buf[self.end..]) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if n == 0 {
                let left = self.end - self.start;
                return if left == 0 {
                    Ok(false)
                } else {
                    Err(ItchError::Truncated(left))
                };
            }
            self.end += n;
        }
        Ok(true)
    }

    /// The next message's bytes (without the length prefix), or `None` at the end.
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>, ItchError> {
        if !self.fill(2)? {
            return Ok(None);
        }
        let len = u16_at(&self.buf, self.start) as usize;
        if !self.fill(2 + len)? {
            unreachable!("fill(2) succeeded, so the stream isn't empty");
        }
        let frame = self.start + 2..self.start + 2 + len;
        self.start = frame.end;
        self.offset += 2 + len as u64;
        self.index += 1;
        Ok(Some(&self.buf[frame]))
    }

    /// The next decoded message, or `None` at the end.
    pub fn next_message(&mut self) -> Result<Option<Message>, ItchError> {
        let (index, offset) = (self.index, self.offset);
        match self.next_frame()? {
            None => Ok(None),
            Some(b) => decode(b).map(Some).map_err(|source| ItchError::Decode {
                index,
                offset,
                source,
            }),
        }
    }
}

/// Open a sample file, decompressing on the fly if its name ends in `.gz`. NASDAQ's
/// files are single gzip members, but a multi-member decoder costs nothing extra.
pub fn open(path: &Path) -> io::Result<Reader<Box<dyn Read>>> {
    let file = File::open(path)?;
    let inner: Box<dyn Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    Ok(Reader::new(inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(locate: u16, timestamp: u64) -> Header {
        Header {
            locate,
            tracking: 7,
            timestamp,
        }
    }

    /// Bytes written out by hand from the spec's field table, independent of `encode`,
    /// so a field offset that's wrong in both `encode` and `decode` still fails here.
    #[test]
    fn decodes_a_hand_written_add_order() {
        let mut b = vec![b'A'];
        b.extend_from_slice(&[0x00, 0x2a]); // locate 42
        b.extend_from_slice(&[0x00, 0x07]); // tracking 7
        b.extend_from_slice(&[0x00, 0x00, 0x1f, 0x48, 0x5b, 0x6c]); // 0x1f485b6c ns
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x01, 0x02]); // order ref 258
        b.push(b'S');
        b.extend_from_slice(&[0, 0, 0x01, 0x2c]); // 300 shares
        b.extend_from_slice(b"AAPL    ");
        b.extend_from_slice(&[0x00, 0x1b, 0x6a, 0x10]); // 1,796,624 = $179.6624
        let msg = decode(&b).unwrap();
        assert_eq!(msg.header, header(42, 0x1f48_5b6c));
        assert_eq!(
            msg.body,
            Body::AddOrder {
                order_ref: 258,
                side: Side::Sell,
                shares: 300,
                stock: Stock::new("AAPL"),
                price: 1_796_624,
            }
        );
    }

    #[test]
    fn decodes_a_hand_written_replace() {
        let mut b = vec![b'U', 0, 1, 0, 0, 0, 0, 0, 0, 0, 9];
        b.extend_from_slice(&5u64.to_be_bytes());
        b.extend_from_slice(&6u64.to_be_bytes());
        b.extend_from_slice(&100u32.to_be_bytes());
        b.extend_from_slice(&12_345u32.to_be_bytes());
        assert_eq!(
            decode(&b).unwrap().body,
            Body::Replace {
                old_ref: 5,
                new_ref: 6,
                shares: 100,
                price: 12_345,
            }
        );
    }

    fn samples() -> Vec<Message> {
        let s = Stock::new("MSFT");
        let bodies = [
            Body::SystemEvent { code: b'Q' },
            Body::StockDirectory { stock: s },
            Body::TradingAction {
                stock: s,
                state: b'T',
            },
            Body::AddOrder {
                order_ref: u64::MAX - 1,
                side: Side::Buy,
                shares: u32::MAX,
                stock: s,
                price: u32::MAX,
            },
            Body::Executed {
                order_ref: 1,
                shares: 2,
                match_number: 3,
            },
            Body::ExecutedWithPrice {
                order_ref: 4,
                shares: 5,
                match_number: 6,
                printable: true,
                price: 7,
            },
            Body::Cancel {
                order_ref: 8,
                shares: 9,
            },
            Body::Delete { order_ref: 10 },
            Body::Replace {
                old_ref: 11,
                new_ref: 12,
                shares: 13,
                price: 14,
            },
            Body::Trade {
                side: Side::Sell,
                shares: 15,
                stock: s,
                price: 16,
                match_number: 17,
            },
            Body::CrossTrade {
                shares: 18,
                stock: s,
                price: 19,
                match_number: 20,
                cross_type: b'O',
            },
        ];
        bodies
            .iter()
            .enumerate()
            .map(|(i, &body)| Message {
                // The largest 48-bit timestamp, to catch a truncated u48.
                header: header(i as u16 + 1, (1 << 48) - 1 - i as u64),
                body,
            })
            .collect()
    }

    #[test]
    fn every_decoded_type_round_trips() {
        for msg in samples() {
            let mut out = Vec::new();
            encode(&msg, &mut out);
            assert_eq!(u16_at(&out, 0) as usize, out.len() - 2);
            assert_eq!(decode(&out[2..]), Ok(msg));
        }
    }

    #[test]
    fn rejects_wrong_lengths_unknown_types_and_bad_sides() {
        let mut out = Vec::new();
        encode(&samples()[3], &mut out);
        let add = &out[2..];
        assert_eq!(
            decode(&add[..35]),
            Err(DecodeError::BadLength {
                kind: b'A',
                expected: 36,
                got: 35
            })
        );
        assert_eq!(decode(&[b'Z', 0]), Err(DecodeError::UnknownType(b'Z')));
        assert_eq!(decode(&[]), Err(DecodeError::Empty));
        let mut bad = add.to_vec();
        bad[19] = b'?';
        assert_eq!(
            decode(&bad),
            Err(DecodeError::BadSide {
                kind: b'A',
                side: b'?'
            })
        );
    }

    #[test]
    fn undecoded_types_are_framed_and_length_checked() {
        let mut b = vec![0u8; 50];
        b[0] = b'I';
        assert_eq!(decode(&b).unwrap().body, Body::Other(b'I'));
        assert!(decode(&b[..49]).is_err());
    }

    /// A reader that returns at most `step` bytes per call, so messages straddle reads.
    struct Dribble<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(out.len()).min(self.data.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn stream(n: usize) -> (Vec<Message>, Vec<u8>) {
        let msgs: Vec<Message> = samples().into_iter().cycle().take(n).collect();
        let mut bytes = Vec::new();
        for m in &msgs {
            encode(m, &mut bytes);
        }
        (msgs, bytes)
    }

    #[test]
    fn reader_handles_messages_split_across_reads() {
        let (msgs, bytes) = stream(50);
        for step in [1, 2, 3, 7, 36, 4096] {
            let mut r = Reader::new(Dribble { data: &bytes, step });
            let mut got = Vec::new();
            while let Some(m) = r.next_message().unwrap() {
                got.push(m);
            }
            assert_eq!(got, msgs, "step {step}");
            assert_eq!(r.offset(), bytes.len() as u64);
        }
    }

    /// More than one buffer's worth, so the slide-to-front path runs with a message
    /// straddling the end of the buffer.
    #[test]
    fn reader_refills_past_its_buffer() {
        let (msgs, bytes) = stream(BUF_LEN / 10);
        assert!(bytes.len() > 2 * BUF_LEN);
        let mut r = Reader::new(Dribble {
            data: &bytes,
            step: 100_003,
        });
        for (i, want) in msgs.iter().enumerate() {
            assert_eq!(
                r.next_message().unwrap().as_ref(),
                Some(want),
                "message {i}"
            );
        }
        assert!(r.next_message().unwrap().is_none());
        assert_eq!(r.count(), msgs.len() as u64);
    }

    #[test]
    fn reader_reports_a_torn_tail_and_where_decoding_failed() {
        let (_, bytes) = stream(3);
        let mut r = Reader::new(&bytes[..bytes.len() - 1]);
        r.next_message().unwrap();
        r.next_message().unwrap();
        assert!(matches!(r.next_message(), Err(ItchError::Truncated(26))));

        // Corrupt the second message's type byte: the error names message 1 at its offset.
        let mut bad = bytes.clone();
        let second = 2 + 12;
        bad[second + 2] = b'Z';
        let mut r = Reader::new(&bad[..]);
        r.next_message().unwrap();
        match r.next_message() {
            Err(ItchError::Decode { index, offset, .. }) => {
                assert_eq!((index, offset), (1, second as u64));
            }
            other => panic!("expected a decode error, got {other:?}"),
        }
    }
}
