//! Any bytes as a session on both books (D22, D89): the fast book emits the reference
//! book's events, both keep their invariants, the ledger balances (shown and hidden
//! quantity, STP, FOK all-or-nothing), and both end in the same state. Four bytes per
//! command, squeezed into a few prices and small quantities so that orders rest, cross,
//! share levels and replenish icebergs all the time.
#![no_main]

use std::num::NonZeroU16;

use libfuzzer_sys::fuzz_target;
use lob::ledger::Ledger;
use lob::{
    BookConfig, Command, FastBook, OrderBook, OrderId, Price, Qty, RefBook, Side, Stp, StpAction,
    TimeInForce,
};

fuzz_target!(|bytes: &[u8]| {
    let config = BookConfig {
        tick_size: 1,
        max_qty: 12,
    };
    let mut reference = RefBook::with_config(config);
    let mut fast = FastBook::with_config(config);
    let mut ledger = Ledger::new();
    let (mut want, mut got) = (Vec::new(), Vec::new());
    let mut next = 1;
    for chunk in bytes.chunks_exact(4) {
        let cmd = command(chunk.try_into().unwrap(), &mut next);
        want.clear();
        got.clear();
        reference.apply(&cmd, &mut want);
        fast.apply(&cmd, &mut got);
        assert_eq!(got, want, "`{cmd}`");
        assert_eq!(fast.check_invariants(), Ok(()), "`{cmd}`");
        assert_eq!(reference.check_invariants(), Ok(()), "`{cmd}`");
        assert_eq!(ledger.observe(&cmd, &want, &reference), Ok(()));
    }
    assert_eq!(fast.state(), reference.state());
});

/// `[kind and side, qty, price, extras]`. New orders take a fresh id unless the top bit of
/// the first byte asks for a recent one; modifies and cancels pick a recent id.
fn command([a, q, p, x]: [u8; 4], next: &mut u64) -> Command {
    let side = if a & 1 == 0 { Side::Buy } else { Side::Sell };
    let qty = Qty(u64::from(q % 14));
    let price = Price(95 + i64::from(p % 11));
    // The k-th most recent id handed out (id 0, never used, before any).
    let recent = |k: u8, next: u64| OrderId(next - 1 - u64::from(k) % next);
    let new_id = |next: &mut u64| {
        if a & 0x80 != 0 {
            recent(a >> 4 & 3, *next)
        } else {
            *next += 1;
            OrderId(*next - 1)
        }
    };
    // Extras: a peak in the low nibble (0..14, or none), an STP group and action above it.
    let peak = (x & 15 >= 2).then(|| Qty(u64::from(x & 15) - 2));
    let actions = [
        StpAction::CancelNewest,
        StpAction::CancelOldest,
        StpAction::CancelBoth,
    ];
    let stp = NonZeroU16::new(u16::from(x >> 4 & 3) % 3).map(|group| Stp {
        group,
        action: actions[usize::from(x >> 6) % 3],
    });
    match a >> 1 & 7 {
        k @ 0..=3 => Command::Limit {
            id: new_id(next),
            side,
            qty,
            price,
            tif: [
                TimeInForce::Gtc,
                TimeInForce::Ioc,
                TimeInForce::Fok,
                TimeInForce::PostOnly,
            ][usize::from(k)],
            peak,
            stp,
        },
        4 => Command::Market {
            id: new_id(next),
            side,
            qty,
            stp,
        },
        5 => Command::Modify {
            id: recent(x, *next),
            qty,
            price,
        },
        _ => Command::Cancel {
            id: recent(x, *next),
        },
    }
}
