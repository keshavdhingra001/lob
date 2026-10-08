//! The fast book against the reference book (D22): identical events for every command,
//! identical depth, and both books' invariants and the conservation ledger, on generated
//! flow and on edge-case flow.
//!
//! The default sizes keep `cargo test` quick. For the full run (millions of commands):
//! `cargo test --release --test differential -- --ignored`

mod common;

use common::random_command;
use lob::gen::{GenConfig, Generator};
use lob::ledger::Ledger;
use lob::rng::Rng;
use lob::{BookConfig, Command, FastBook, OrderBook, Price, RefBook, Side};

/// Feed both books the same commands; fail at the first difference.
fn compare(
    label: &str,
    config: BookConfig,
    commands: impl Iterator<Item = Command>,
    check_every: usize,
) {
    let mut reference = RefBook::with_config(config);
    let mut fast = FastBook::with_config(config);
    let mut ledger = Ledger::new();
    let (mut ref_events, mut fast_events) = (Vec::new(), Vec::new());
    for (n, cmd) in commands.enumerate() {
        ref_events.clear();
        fast_events.clear();
        reference.apply(&cmd, &mut ref_events);
        fast.apply(&cmd, &mut fast_events);
        let fail = |e: String| panic!("{label}, command #{n} `{cmd}`: {e}");
        if ref_events != fast_events {
            fail(format!(
                "events differ\n  reference: {ref_events:?}\n  fast:      {fast_events:?}"
            ));
        }
        if n % check_every == 0 {
            for side in [Side::Buy, Side::Sell] {
                let (r, f) = (
                    reference.depth(side, usize::MAX),
                    fast.depth(side, usize::MAX),
                );
                if r != f {
                    fail(format!(
                        "{side} depth differs\n  reference: {r:?}\n  fast:      {f:?}"
                    ));
                }
                // A limited query too: the top 3 levels, and none at all.
                for n in [0, 3] {
                    let (r, f) = (reference.depth(side, n), fast.depth(side, n));
                    if r != f {
                        fail(format!(
                            "{side} depth({n}) differs\n  reference: {r:?}\n  fast:      {f:?}"
                        ));
                    }
                }
            }
            fast.check_invariants().unwrap_or_else(fail);
            reference.check_invariants().unwrap_or_else(fail);
            // Depth shows only icebergs' slices; the states hold their hidden parts too.
            if reference.state() != fast.state() {
                fail("book states differ".to_string());
            }
            ledger
                .observe(&cmd, &fast_events, &fast)
                .unwrap_or_else(fail);
        } else {
            // The ledger must see every command's events; its book comparison is the costly part.
            ledger
                .observe(&cmd, &fast_events, &fast)
                .unwrap_or_else(fail);
        }
    }
}

/// What new orders in the edge-case flows may carry: STP groups (0: none) and icebergs.
#[derive(Clone, Copy)]
struct Mix {
    groups: u16,
    icebergs: bool,
}

const PLAIN: Mix = Mix {
    groups: 0,
    icebergs: false,
};
const GROUPED: Mix = Mix {
    groups: 2,
    icebergs: false,
};
const ICEBERGS: Mix = Mix {
    groups: 0,
    icebergs: true,
};
const BOTH: Mix = Mix {
    groups: 2,
    icebergs: true,
};

fn edge_cases(seed: u64, n: usize, tick: i64, mix: Mix) -> impl Iterator<Item = Command> {
    let mut rng = Rng::new(seed);
    let mut next_id = 1;
    (0..n).map(move |_| random_command(&mut rng, &mut next_id, tick, mix.groups, mix.icebergs))
}

/// Edge-case flow with prices spread over the fast book's ladder window (D33): mostly near
/// the first price, but also straddling both window edges and far outside it (the overflow
/// tree). Matching then runs across the window/overflow boundary in both directions.
fn wide(seed: u64, n: usize, tick: i64, mix: Mix) -> impl Iterator<Item = Command> {
    let half = (lob::ladder::WINDOW / 2) as i64 * tick;
    let mut rng = Rng::new(seed ^ 0x5a5a_5a5a);
    edge_cases(seed, n, tick, mix).map(move |cmd| {
        let shift = match rng.below(10) {
            0 => half,
            1 => -half,
            2 => 3 * half,
            3 => -3 * half,
            _ => 0,
        };
        let moved = |p: Price| Price(p.0 + shift);
        match cmd {
            Command::Limit {
                id,
                side,
                qty,
                price,
                tif,
                peak,
                stp,
            } => Command::Limit {
                id,
                side,
                qty,
                price: moved(price),
                tif,
                peak,
                stp,
            },
            Command::Modify { id, qty, price } => Command::Modify {
                id,
                qty,
                price: moved(price),
            },
            other => other,
        }
    })
}

fn edge_config(tick: i64) -> BookConfig {
    BookConfig {
        tick_size: tick,
        max_qty: 12,
    }
}

#[test]
fn generated_flow_matches_reference() {
    for seed in 1..=5 {
        compare(
            &format!("generated seed {seed}"),
            BookConfig::default(),
            Generator::seeded(seed).take(40_000),
            10,
        );
    }
}

#[test]
fn edge_case_flow_matches_reference() {
    for seed in 0..20 {
        compare(
            &format!("edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 10_000, 1, PLAIN),
            1,
        );
    }
    for seed in 100..110 {
        compare(
            &format!("edge tick-5 seed {seed}"),
            edge_config(5),
            edge_cases(seed, 10_000, 5, PLAIN),
            1,
        );
    }
}

#[test]
fn wide_price_flow_matches_reference() {
    for (tick, seeds) in [(1, 0..10), (5, 50..55)] {
        for seed in seeds {
            compare(
                &format!("wide tick-{tick} seed {seed}"),
                edge_config(tick),
                wide(seed, 10_000, tick, PLAIN),
                1,
            );
        }
    }
}

/// The wide flow really does reach the overflow tree, so the test above covers it.
#[test]
fn wide_price_flow_uses_the_overflow() {
    let mut book = FastBook::with_config(edge_config(1));
    let mut events = Vec::new();
    let mut most = 0;
    for cmd in wide(0, 10_000, 1, PLAIN) {
        events.clear();
        book.apply(&cmd, &mut events);
        most = most.max(book.overflow_levels());
    }
    assert!(most >= 4, "at most {most} overflow levels");
}

/// Generated flow where new orders carry one of 3 STP groups (or none).
fn grouped(seed: u64) -> Generator {
    Generator::new(GenConfig {
        stp_groups: 3,
        ..GenConfig::with_seed(seed)
    })
}

/// Self-trade prevention (D70, D71) on every flow, with few groups so that self-trades are
/// attempted all the time.
#[test]
fn stp_flow_matches_reference() {
    for seed in 1..=3 {
        compare(
            &format!("grouped seed {seed}"),
            BookConfig::default(),
            grouped(seed).take(40_000),
            10,
        );
    }
    for seed in 0..20 {
        compare(
            &format!("grouped edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 10_000, 1, GROUPED),
            1,
        );
    }
    for seed in 0..5 {
        compare(
            &format!("grouped wide seed {seed}"),
            edge_config(1),
            wide(seed, 10_000, 1, GROUPED),
            1,
        );
    }
}

/// Generated flow where a third of the big passive orders are icebergs.
fn icebergs(seed: u64, stp_groups: u16) -> Generator {
    Generator::new(GenConfig {
        stp_groups,
        iceberg_pct: 33,
        ..GenConfig::with_seed(seed)
    })
}

/// Icebergs (D83-D88), alone and with STP groups, on every flow.
#[test]
fn iceberg_flow_matches_reference() {
    for seed in 1..=3 {
        for groups in [0, 3] {
            compare(
                &format!("iceberg seed {seed} groups {groups}"),
                BookConfig::default(),
                icebergs(seed, groups).take(40_000),
                10,
            );
        }
    }
    for seed in 0..20 {
        for mix in [ICEBERGS, BOTH] {
            compare(
                &format!("iceberg edge seed {seed} groups {}", mix.groups),
                edge_config(1),
                edge_cases(seed, 10_000, 1, mix),
                1,
            );
        }
    }
    for seed in 0..5 {
        compare(
            &format!("iceberg wide seed {seed}"),
            edge_config(1),
            wide(seed, 10_000, 1, BOTH),
            1,
        );
    }
}

#[test]
fn deep_queue_matches_reference() {
    compare(
        "deep queue",
        BookConfig::default(),
        lob::gen::deep_queue(3_000, 9).into_iter(),
        97,
    );
}

#[test]
#[ignore = "millions of commands; run with --release -- --ignored"]
fn long_differential_run() {
    for seed in 1..=10 {
        compare(
            &format!("generated seed {seed}"),
            BookConfig::default(),
            Generator::seeded(seed).take(1_000_000),
            1_000,
        );
    }
    for seed in 0..10 {
        compare(
            &format!("edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 500_000, 1, PLAIN),
            1_000,
        );
        compare(
            &format!("wide seed {seed}"),
            edge_config(1),
            wide(seed, 500_000, 1, PLAIN),
            1_000,
        );
        compare(
            &format!("grouped edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 500_000, 1, GROUPED),
            1_000,
        );
        compare(
            &format!("grouped seed {seed}"),
            BookConfig::default(),
            grouped(seed).take(1_000_000),
            1_000,
        );
        compare(
            &format!("iceberg edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 500_000, 1, BOTH),
            1_000,
        );
        compare(
            &format!("iceberg seed {seed}"),
            BookConfig::default(),
            icebergs(seed, 3).take(1_000_000),
            1_000,
        );
    }
}
