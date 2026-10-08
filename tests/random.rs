//! Random command streams against the reference book, checking the invariants and the
//! conservation ledger after every command. Narrow prices and small quantities make
//! crossing, partial fills, sweeps and FOK edge cases common.

mod common;

use common::random_command;
use lob::ledger::Ledger;
use lob::rng::Rng;
use lob::{BookConfig, OrderBook, RefBook};

fn run(seed: u64, commands: usize, tick: i64, groups: u16, icebergs: bool) {
    let mut rng = Rng::new(seed);
    let mut book = RefBook::with_config(BookConfig {
        tick_size: tick,
        max_qty: 12,
    });
    let mut ledger = Ledger::new();
    let mut next_id = 1;
    let mut events = Vec::new();
    let (mut trades, mut replenished) = (0, 0);
    for n in 0..commands {
        let cmd = random_command(&mut rng, &mut next_id, tick, groups, icebergs);
        events.clear();
        book.apply(&cmd, &mut events);
        let fail = |e: String| panic!("seed {seed}, command #{n} `{cmd}`: {e}\nevents: {events:?}");
        book.check_invariants().unwrap_or_else(fail);
        ledger.observe(&cmd, &events, &book).unwrap_or_else(fail);
        trades += events
            .iter()
            .filter(|e| matches!(e, lob::Event::Trade { .. }))
            .count();
        replenished += events
            .iter()
            .filter(|e| matches!(e, lob::Event::Replenished { .. }))
            .count();
    }
    // Guard against a generator that stopped exercising matching.
    assert!(
        trades > commands / 10,
        "only {trades} trades in {commands} commands"
    );
    assert!(
        !icebergs || replenished > commands / 50,
        "only {replenished} replenishes in {commands} commands"
    );
}

#[test]
fn random_sessions_conserve_quantity() {
    for seed in 0..20 {
        run(seed, 5_000, 1, 0, false);
    }
}

#[test]
fn random_sessions_with_a_coarse_tick() {
    for seed in 100..110 {
        run(seed, 5_000, 5, 0, false);
    }
}

/// With STP groups the ledger also checks D70 from the outside: no self-trade prints, and
/// every STP cancel is one the taker's action allows.
#[test]
fn random_sessions_with_stp_groups() {
    for seed in 200..220 {
        run(seed, 5_000, 1, 2, false);
    }
}

/// Icebergs (D83-D88), alone and with STP groups: the ledger checks that trades take only
/// shown quantity, each slice is `min(peak, hidden)`, and depth equals the shown parts.
#[test]
fn random_sessions_with_icebergs() {
    for seed in 300..320 {
        run(seed, 5_000, 1, 0, true);
        run(seed, 5_000, 1, 2, true);
    }
}
