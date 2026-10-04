//! Random command streams against the reference book, checking the invariants and the
//! conservation ledger after every command. Narrow prices and small quantities make
//! crossing, partial fills, sweeps and FOK edge cases common.

use lob::command::TimeInForce;
use lob::ledger::Ledger;
use lob::rng::Rng;
use lob::{BookConfig, Command, OrderBook, OrderId, Price, Qty, RefBook, Side};

/// Mostly a fresh id, sometimes an old one (to hit duplicate-id rejects).
fn new_order_id(rng: &mut Rng, next_id: &mut u64) -> OrderId {
    if *next_id > 1 && rng.chance(10) {
        OrderId(1 + rng.below(*next_id - 1))
    } else {
        *next_id += 1;
        OrderId(*next_id - 1)
    }
}

fn random_command(rng: &mut Rng, next_id: &mut u64, tick: i64) -> Command {
    let side = if rng.chance(50) {
        Side::Buy
    } else {
        Side::Sell
    };
    // Some quantities are 0 or above the max (12), some prices off the tick grid.
    let qty = Qty(rng.below(14));
    let price = Price(if rng.chance(5) {
        rng.range(90, 110)
    } else {
        rng.range(95, 105) * tick
    });
    match rng.below(100) {
        0..=44 => {
            let tif = match rng.below(10) {
                0 => TimeInForce::Ioc,
                1 => TimeInForce::Fok,
                2 => TimeInForce::PostOnly,
                _ => TimeInForce::Gtc,
            };
            Command::Limit {
                id: new_order_id(rng, next_id),
                side,
                qty,
                price,
                tif,
            }
        }
        45..=54 => Command::Market {
            id: new_order_id(rng, next_id),
            side,
            qty,
        },
        55..=74 => Command::Modify {
            id: OrderId(1 + rng.below(*next_id)),
            qty,
            price,
        },
        _ => Command::Cancel {
            id: OrderId(1 + rng.below(*next_id)),
        },
    }
}

fn run(seed: u64, commands: usize, tick: i64) {
    let mut rng = Rng::new(seed);
    let mut book = RefBook::with_config(BookConfig {
        tick_size: tick,
        max_qty: 12,
    });
    let mut ledger = Ledger::new();
    let mut next_id = 1;
    let mut events = Vec::new();
    let mut trades = 0;
    for n in 0..commands {
        let cmd = random_command(&mut rng, &mut next_id, tick);
        events.clear();
        book.apply(&cmd, &mut events);
        let fail = |e: String| panic!("seed {seed}, command #{n} `{cmd}`: {e}\nevents: {events:?}");
        book.check_invariants().unwrap_or_else(fail);
        ledger.observe(&cmd, &events, &book).unwrap_or_else(fail);
        trades += events
            .iter()
            .filter(|e| matches!(e, lob::Event::Trade { .. }))
            .count();
    }
    // Guard against a generator that stopped exercising matching.
    assert!(
        trades > commands / 10,
        "only {trades} trades in {commands} commands"
    );
}

#[test]
fn random_sessions_conserve_quantity() {
    for seed in 0..20 {
        run(seed, 5_000, 1);
    }
}

#[test]
fn random_sessions_with_a_coarse_tick() {
    for seed in 100..110 {
        run(seed, 5_000, 5);
    }
}
