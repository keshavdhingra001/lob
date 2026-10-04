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
use lob::{BookConfig, Command, FastBook, OrderBook, RefBook, Side};

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
            }
            fast.check_invariants().unwrap_or_else(fail);
            reference.check_invariants().unwrap_or_else(fail);
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

fn generated(seed: u64, n: usize) -> impl Iterator<Item = Command> {
    Generator::new(GenConfig {
        seed,
        ..GenConfig::default()
    })
    .take(n)
}

fn edge_cases(seed: u64, n: usize, tick: i64) -> impl Iterator<Item = Command> {
    let mut rng = Rng::new(seed);
    let mut next_id = 1;
    (0..n).map(move |_| random_command(&mut rng, &mut next_id, tick))
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
            generated(seed, 40_000),
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
            edge_cases(seed, 10_000, 1),
            1,
        );
    }
    for seed in 100..110 {
        compare(
            &format!("edge tick-5 seed {seed}"),
            edge_config(5),
            edge_cases(seed, 10_000, 5),
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
            generated(seed, 1_000_000),
            1_000,
        );
    }
    for seed in 0..10 {
        compare(
            &format!("edge seed {seed}"),
            edge_config(1),
            edge_cases(seed, 500_000, 1),
            1_000,
        );
    }
}
