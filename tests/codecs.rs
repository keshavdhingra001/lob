//! Property tests for every codec (D49, D50): round trips over every field value, decoders
//! that never panic on any bytes, and a journal that never returns commands it wasn't given.
//! `PROPTEST_CASES=<n>` sets the number of cases (default 256).

use lob::book::Level;
use lob::feed::{self, Msg, Snapshot};
use lob::itch::{self, Body, Header, Message, Stock};
use lob::journal::{decode_command, encode_command, read_journal, JournalWriter};
use lob::{Command, OrderId, Price, Qty, Side, TimeInForce};
use proptest::prelude::*;

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn command() -> impl Strategy<Value = Command> {
    let tif = prop_oneof![
        Just(TimeInForce::Gtc),
        Just(TimeInForce::Ioc),
        Just(TimeInForce::Fok),
        Just(TimeInForce::PostOnly),
    ];
    let (id, qty, price) = (any::<u64>(), any::<u64>(), any::<i64>());
    prop_oneof![
        (id, side(), qty, price, tif).prop_map(|(id, side, qty, price, tif)| Command::Limit {
            id: OrderId(id),
            side,
            qty: Qty(qty),
            price: Price(price),
            tif,
        }),
        (id, side(), qty).prop_map(|(id, side, qty)| Command::Market {
            id: OrderId(id),
            side,
            qty: Qty(qty),
        }),
        (id, qty, price).prop_map(|(id, qty, price)| Command::Modify {
            id: OrderId(id),
            qty: Qty(qty),
            price: Price(price),
        }),
        id.prop_map(|id| Command::Cancel { id: OrderId(id) }),
    ]
}

/// One edit to a byte string: overwrite, flip a bit, insert, delete, or cut the rest.
/// Positions wrap, so every edit applies to any non-empty input.
#[derive(Clone, Debug)]
enum Edit {
    Set(usize, u8),
    Flip(usize, u8),
    Insert(usize, u8),
    Delete(usize),
    Truncate(usize),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    let edit = prop_oneof![
        (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Edit::Set(i, b)),
        (any::<usize>(), 0..8u8).prop_map(|(i, bit)| Edit::Flip(i, bit)),
        (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Edit::Insert(i, b)),
        any::<usize>().prop_map(Edit::Delete),
        any::<usize>().prop_map(Edit::Truncate),
    ];
    prop::collection::vec(edit, 0..4)
}

fn apply(mut bytes: Vec<u8>, edits: &[Edit]) -> Vec<u8> {
    for edit in edits {
        let n = bytes.len();
        match *edit {
            Edit::Insert(i, b) => bytes.insert(i % (n + 1), b),
            _ if n == 0 => {}
            Edit::Set(i, b) => bytes[i % n] = b,
            Edit::Flip(i, bit) => bytes[i % n] ^= 1 << bit,
            Edit::Delete(i) => {
                bytes.remove(i % n);
            }
            Edit::Truncate(i) => bytes.truncate(i % n),
        }
    }
    bytes
}

/// Mostly a valid encoding with a few edits (it gets past the first checks), sometimes
/// plain random bytes (D49).
fn near(valid: impl Strategy<Value = Vec<u8>>) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => (valid, edits()).prop_map(|(bytes, e)| apply(bytes, &e)),
        1 => prop::collection::vec(any::<u8>(), 0..64),
    ]
}

fn command_bytes() -> impl Strategy<Value = Vec<u8>> {
    command().prop_map(|cmd| {
        let mut buf = [0; 32];
        let len = encode_command(&cmd, &mut buf);
        buf[..len].to_vec()
    })
}

fn journal(commands: &[Command]) -> Vec<u8> {
    let mut w = JournalWriter::new(Vec::new()).unwrap();
    for cmd in commands {
        w.append(cmd).unwrap();
    }
    w.finish().unwrap()
}

/// The offsets where each record ends (the header is 8 bytes; a record is 6 + payload).
fn record_ends(commands: &[Command]) -> Vec<usize> {
    let mut end = 8;
    commands
        .iter()
        .map(|cmd| {
            end += 6 + encode_command(cmd, &mut [0; 32]);
            end
        })
        .collect()
}

fn level() -> impl Strategy<Value = Level> {
    (any::<i64>(), any::<u64>(), any::<u32>()).prop_map(|(p, q, o)| Level {
        price: Price(p),
        qty: Qty(q),
        orders: o as usize,
    })
}

fn msg() -> impl Strategy<Value = Msg> {
    prop_oneof![
        (any::<u64>(), side(), level(), any::<bool>()).prop_map(|(seq, side, level, last)| {
            Msg::Level {
                seq,
                side,
                level,
                last,
            }
        }),
        (
            any::<u64>(),
            side(),
            any::<i64>(),
            any::<u64>(),
            any::<bool>()
        )
            .prop_map(|(seq, aggressor, price, qty, last)| Msg::Trade {
                seq,
                aggressor,
                price: Price(price),
                qty: Qty(qty),
                last,
            }),
    ]
}

fn snapshot() -> impl Strategy<Value = Snapshot> {
    let levels = || prop::collection::vec(level(), 0..6);
    (any::<u64>(), levels(), levels()).prop_map(|(seq, bids, asks)| Snapshot { seq, bids, asks })
}

fn itch_message() -> impl Strategy<Value = Message> {
    let stock = any::<[u8; 8]>().prop_map(Stock);
    let header = (any::<u16>(), any::<u16>(), 0..1u64 << 48).prop_map(|(l, t, ts)| Header {
        locate: l,
        tracking: t,
        timestamp: ts,
    });
    let (r, n, p, m) = (any::<u64>(), any::<u32>(), any::<u32>(), any::<u64>());
    let body = prop_oneof![
        any::<u8>().prop_map(|code| Body::SystemEvent { code }),
        stock
            .clone()
            .prop_map(|stock| Body::StockDirectory { stock }),
        (stock.clone(), any::<u8>())
            .prop_map(|(stock, state)| Body::TradingAction { stock, state }),
        (r, side(), n, stock.clone(), p).prop_map(|(order_ref, side, shares, stock, price)| {
            Body::AddOrder {
                order_ref,
                side,
                shares,
                stock,
                price,
            }
        }),
        (r, n, m).prop_map(|(order_ref, shares, match_number)| Body::Executed {
            order_ref,
            shares,
            match_number,
        }),
        (r, n, m, any::<bool>(), p).prop_map(
            |(order_ref, shares, match_number, printable, price)| {
                Body::ExecutedWithPrice {
                    order_ref,
                    shares,
                    match_number,
                    printable,
                    price,
                }
            }
        ),
        (r, n).prop_map(|(order_ref, shares)| Body::Cancel { order_ref, shares }),
        r.prop_map(|order_ref| Body::Delete { order_ref }),
        (r, r, n, p).prop_map(|(old_ref, new_ref, shares, price)| Body::Replace {
            old_ref,
            new_ref,
            shares,
            price,
        }),
        (side(), n, stock.clone(), p, m).prop_map(|(side, shares, stock, price, match_number)| {
            Body::Trade {
                side,
                shares,
                stock,
                price,
                match_number,
            }
        }),
        (any::<u64>(), stock, p, m, any::<u8>()).prop_map(
            |(shares, stock, price, match_number, cross_type)| Body::CrossTrade {
                shares,
                stock,
                price,
                match_number,
                cross_type,
            }
        ),
    ];
    (header, body).prop_map(|(header, body)| Message { header, body })
}

/// Hands out its bytes a few at a time, so frames straddle reads.
struct Chunked<'a>(&'a [u8], usize);

impl std::io::Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.len().min(buf.len()).min(self.1);
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

proptest! {
    #[test]
    fn commands_round_trip_through_text(cmd in command()) {
        prop_assert_eq!(cmd.to_string().parse::<Command>(), Ok(cmd));
    }

    #[test]
    fn command_lines_never_panic(
        words in prop::collection::vec(prop_oneof![
            prop::sample::select(vec![
                "limit", "market", "modify", "cancel", "buy", "sell", "gtc", "ioc", "fok", "post",
                "-", "", "1e3", "0x10", "18446744073709551616", "-9223372036854775809",
            ]).prop_map(str::to_string),
            any::<i64>().prop_map(|n| n.to_string()),
            any::<String>(),
        ], 0..7),
        sep in prop::sample::select(vec![" ", "  ", "\t", " \u{a0}"]),
    ) {
        let _ = words.join(sep).parse::<Command>();
    }

    #[test]
    fn commands_round_trip_through_the_journal_encoding(cmd in command()) {
        let mut buf = [0; 32];
        let len = encode_command(&cmd, &mut buf);
        prop_assert_eq!(decode_command(&buf[..len]), Ok(cmd));
    }

    /// Any accepted payload is the canonical encoding of what it decodes to: no
    /// trailing bytes, no second spelling of a side or time in force.
    #[test]
    fn accepted_command_payloads_are_canonical(bytes in near(command_bytes())) {
        if let Ok(cmd) = decode_command(&bytes) {
            let mut buf = [0; 32];
            let len = encode_command(&cmd, &mut buf);
            prop_assert_eq!(&buf[..len], &bytes[..]);
        }
    }

    /// Cut a journal anywhere: the whole records before the cut come back, and a torn
    /// tail is reported exactly when the cut falls inside a record.
    #[test]
    fn a_cut_journal_returns_the_records_before_the_cut(
        commands in prop::collection::vec(command(), 0..12),
        cut in any::<prop::sample::Index>(),
    ) {
        let bytes = journal(&commands);
        let cut = 8 + cut.index(bytes.len() - 8 + 1);
        let ends = record_ends(&commands);
        let whole = ends.iter().take_while(|&&end| end <= cut).count();
        let j = read_journal(&bytes[..cut]).unwrap();
        prop_assert_eq!(&j.commands[..], &commands[..whole]);
        let boundary = if whole == 0 { 8 } else { ends[whole - 1] };
        let torn = (cut != boundary).then_some(boundary as u64);
        prop_assert_eq!(j.torn_tail, torn);
    }

    /// Flip any one bit: the journal fails, or returns a prefix of what was written,
    /// and all of it when it reports no torn tail. It never returns a changed command.
    #[test]
    fn a_flipped_bit_never_changes_a_command(
        commands in prop::collection::vec(command(), 1..12),
        at in any::<prop::sample::Index>(),
        bit in 0..8u8,
    ) {
        let mut bytes = journal(&commands);
        let at = at.index(bytes.len());
        bytes[at] ^= 1 << bit;
        if let Ok(j) = read_journal(&bytes) {
            prop_assert!(commands.starts_with(&j.commands));
            if j.torn_tail.is_none() {
                prop_assert_eq!(j.commands, commands);
            }
        }
    }

    #[test]
    fn journals_never_panic(bytes in near((prop::collection::vec(command(), 0..4)).prop_map(|c| journal(&c)))) {
        let _ = read_journal(&bytes);
    }

    #[test]
    fn feed_messages_round_trip(m in msg()) {
        let mut buf = Vec::new();
        feed::encode(&m, &mut buf);
        prop_assert_eq!(feed::decode(&buf), Ok((m, buf.len())));
    }

    /// Whatever the bytes, `decode` reports a length within them, and re-encoding what it
    /// decoded gives back exactly those bytes.
    #[test]
    fn accepted_feed_messages_are_canonical(bytes in near(msg().prop_map(|m| {
        let mut buf = Vec::new();
        feed::encode(&m, &mut buf);
        buf
    }))) {
        if let Ok((m, len)) = feed::decode(&bytes) {
            let mut buf = Vec::new();
            feed::encode(&m, &mut buf);
            prop_assert_eq!(&buf[..], &bytes[..len]);
        }
    }

    #[test]
    fn snapshots_round_trip(snap in snapshot()) {
        let mut buf = Vec::new();
        feed::encode_snapshot(&snap, &mut buf);
        prop_assert_eq!(feed::decode_snapshot(&buf), Ok(snap));
    }

    #[test]
    fn accepted_snapshots_are_canonical(bytes in near(snapshot().prop_map(|s| {
        let mut buf = Vec::new();
        feed::encode_snapshot(&s, &mut buf);
        buf
    }))) {
        if let Ok(snap) = feed::decode_snapshot(&bytes) {
            let mut buf = Vec::new();
            feed::encode_snapshot(&snap, &mut buf);
            prop_assert_eq!(buf, bytes);
        }
    }

    #[test]
    fn itch_messages_round_trip(m in itch_message()) {
        let mut buf = Vec::new();
        itch::encode(&m, &mut buf);
        let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        prop_assert_eq!(len, buf.len() - 2);
        prop_assert_eq!(itch::decode(&buf[2..]), Ok(m));
    }

    /// An ITCH stream of any bytes, read a few bytes at a time: the reader returns
    /// messages, a clean end or an error, and never panics or loops.
    #[test]
    fn itch_streams_never_panic(
        bytes in near(prop::collection::vec(itch_message(), 0..4).prop_map(|ms| {
            let mut buf = Vec::new();
            for m in &ms {
                itch::encode(m, &mut buf);
            }
            buf
        })),
        chunk in 1..9usize,
    ) {
        let mut reader = itch::Reader::new(Chunked(&bytes, chunk));
        let mut consumed = 0;
        while let Ok(Some(_)) = reader.next_message() {
            prop_assert!(reader.offset() > consumed, "no progress");
            consumed = reader.offset();
        }
        prop_assert!(reader.offset() <= bytes.len() as u64);
    }
}
