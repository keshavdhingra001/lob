//! Property tests for the engine and the feed (D50). A session is a list of operations
//! whose ids are assigned when it's built, so shrinking can drop any operation and still
//! leave a meaningful session: a failure comes back as a handful of commands.

use lob::consumer::{Action, Consumer};
use lob::feed::Publisher;
use lob::ledger::Ledger;
use std::num::NonZeroU16;

use lob::{
    BookConfig, Command, FastBook, OrderBook, OrderId, Price, Qty, RefBook, Side, Stp, StpAction,
    TimeInForce,
};
use proptest::prelude::*;

const CONFIG: BookConfig = BookConfig {
    tick_size: 1,
    max_qty: 12,
};

/// Which id a new order gets: the next fresh one, or the k-th most recent one (rejected as
/// not increasing).
#[derive(Clone, Copy, Debug)]
enum NewId {
    Fresh,
    Recent(u64),
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Limit(NewId, Side, u64, i64, TimeInForce, Option<u64>, Option<Stp>),
    Market(NewId, Side, u64, Option<Stp>),
    /// Targets are the k-th most recent id (0: the latest), so some are finished or were
    /// never accepted. Relative targets keep their meaning when shrinking removes an
    /// earlier operation, which absolute ids wouldn't.
    Modify(u64, u64, i64),
    Cancel(u64),
}

fn op() -> impl Strategy<Value = Op> {
    // proptest shrinks a union towards its first branch, so the common case comes first.
    let new_id = prop_oneof![9 => Just(NewId::Fresh), 1 => (0..4u64).prop_map(NewId::Recent)];
    let side = prop_oneof![Just(Side::Buy), Just(Side::Sell)];
    let tif = prop_oneof![
        6 => Just(TimeInForce::Gtc),
        1 => Just(TimeInForce::Ioc),
        1 => Just(TimeInForce::Fok),
        1 => Just(TimeInForce::PostOnly),
    ];
    // Quantities up to 13 (12 is the max); 0 (rejected) is rare and last, so shrinking
    // makes orders smaller rather than turning them into rejects. Prices around 100.
    let qty = prop_oneof![12 => 1..14u64, 1 => Just(0u64)];
    let price = 95..106i64;
    let target = 0..8u64;
    // No group first (it shrinks there), else one of two groups (D67).
    let action = prop_oneof![
        Just(StpAction::CancelNewest),
        Just(StpAction::CancelOldest),
        Just(StpAction::CancelBoth),
    ];
    let stp = prop_oneof![
        2 => Just(None),
        1 => (1..=2u16, action).prop_map(|(g, action)| Some(Stp {
            group: NonZeroU16::new(g).unwrap(),
            action,
        })),
    ];
    // No peak first; else 0..14, so valid icebergs and every bad-peak case occur (D83).
    let peak = prop_oneof![3 => Just(None), 1 => (0..14u64).prop_map(Some)];
    prop_oneof![
        5 => (new_id.clone(), side.clone(), qty.clone(), price.clone(), tif, peak, stp.clone())
            .prop_map(|(i, s, q, p, t, k, g)| Op::Limit(i, s, q, p, t, k, g)),
        1 => (new_id, side, qty.clone(), stp).prop_map(|(i, s, q, g)| Op::Market(i, s, q, g)),
        2 => (target.clone(), qty, price).prop_map(|(t, q, p)| Op::Modify(t, q, p)),
        2 => target.prop_map(Op::Cancel),
    ]
}

fn new_id(id: NewId, next: &mut u64) -> OrderId {
    match id {
        NewId::Fresh => {
            *next += 1;
            OrderId(*next - 1)
        }
        NewId::Recent(k) => recent(k, *next),
    }
}

/// The k-th most recent id handed out (0 is the latest); id 0, never used, before any.
fn recent(k: u64, next: u64) -> OrderId {
    OrderId(next - 1 - k % next)
}

fn session(ops: &[Op]) -> Vec<Command> {
    let mut next = 1;
    let mut commands = Vec::with_capacity(ops.len());
    for &op in ops {
        commands.push(match op {
            Op::Limit(id, side, qty, price, tif, peak, stp) => Command::Limit {
                id: new_id(id, &mut next),
                side,
                qty: Qty(qty),
                price: Price(price),
                tif,
                peak: peak.map(Qty),
                stp,
            },
            Op::Market(id, side, qty, stp) => Command::Market {
                id: new_id(id, &mut next),
                side,
                qty: Qty(qty),
                stp,
            },
            Op::Modify(t, qty, price) => Command::Modify {
                id: recent(t, next),
                qty: Qty(qty),
                price: Price(price),
            },
            Op::Cancel(t) => Command::Cancel {
                id: recent(t, next),
            },
        });
    }
    commands
}

fn sessions() -> impl Strategy<Value = Vec<Command>> {
    prop::collection::vec(op(), 0..150).prop_map(|ops| session(&ops))
}

fn depth<B: OrderBook>(book: &B) -> [Vec<lob::Level>; 2] {
    [Side::Buy, Side::Sell].map(|s| book.depth(s, usize::MAX))
}

proptest! {
    /// D22 as a property: the fast book emits the reference book's events, both keep their
    /// invariants, and the ledger balances, after every command.
    #[test]
    fn the_books_agree_and_conserve_quantity(commands in sessions()) {
        let mut reference = RefBook::with_config(CONFIG);
        let mut fast = FastBook::with_config(CONFIG);
        let mut ledger = Ledger::new();
        let (mut want, mut got) = (Vec::new(), Vec::new());
        for (n, cmd) in commands.iter().enumerate() {
            want.clear();
            got.clear();
            reference.apply(cmd, &mut want);
            fast.apply(cmd, &mut got);
            prop_assert_eq!(&got, &want, "command #{} `{}`", n, cmd);
            prop_assert_eq!(fast.check_invariants(), Ok(()), "command #{} `{}`", n, cmd);
            prop_assert_eq!(reference.check_invariants(), Ok(()));
            prop_assert_eq!(ledger.observe(cmd, &want, &reference), Ok(()));
        }
        prop_assert_eq!(depth(&fast), depth(&reference));
        // Same logical book, order by order (D74), so either one's snapshot fits the other.
        prop_assert_eq!(fast.state(), reference.state());
    }

    /// For any session and any pattern of lost messages, a consumer that gets a heartbeat
    /// after each command and a snapshot whenever it asks shows the engine's book after
    /// every command (D42).
    #[test]
    fn a_consumer_survives_any_loss_pattern(
        commands in sessions(),
        lost in prop::collection::vec(any::<bool>(), 1..40),
    ) {
        let mut book = FastBook::with_config(CONFIG);
        let mut publisher = Publisher::new();
        let mut consumer = Consumer::new();
        let (mut events, mut msgs) = (Vec::new(), Vec::new());
        let mut sent = 0;
        for (n, cmd) in commands.iter().enumerate() {
            events.clear();
            msgs.clear();
            book.apply(cmd, &mut events);
            publisher.on_command(cmd, &events, &mut msgs).unwrap();
            let mut asked = false;
            for &msg in &msgs {
                sent += 1;
                if !lost[sent % lost.len()] {
                    asked |= consumer.on_msg(msg) == Action::RequestSnapshot;
                }
            }
            asked |= consumer.on_heartbeat(publisher.seq()) == Action::RequestSnapshot;
            if asked {
                prop_assert_eq!(consumer.on_snapshot(&publisher.snapshot()), Action::Continue);
            }
            prop_assert!(consumer.is_consistent(), "command #{} `{}`", n, cmd);
            let shown = [Side::Buy, Side::Sell].map(|s| consumer.depth(s, usize::MAX));
            prop_assert_eq!(shown, depth(&book), "command #{} `{}`", n, cmd);
        }
    }
}
