//! Deterministic replay end to end: generator -> journal -> replay -> digest.

use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, JournalWriter};
use lob::ledger::Ledger;
use lob::replay::replay;
use lob::{Command, Event, OrderBook, RefBook, StpAction};

fn generate(seed: u64, n: usize) -> Vec<Command> {
    Generator::seeded(seed).take(n).collect()
}

fn event_bytes(commands: &[Command]) -> (lob::replay::ReplayStats, Vec<u8>) {
    let mut bytes = Vec::new();
    let stats = replay(&mut RefBook::new(), commands, |b| {
        bytes.extend_from_slice(b)
    });
    (stats, bytes)
}

#[test]
fn replay_is_byte_identical_across_runs() {
    let commands = generate(7, 50_000);
    let (a, bytes_a) = event_bytes(&commands);
    let (b, bytes_b) = event_bytes(&commands);
    assert_eq!(a, b);
    assert!(bytes_a == bytes_b, "event streams differ");
    assert!(a.trades > 5_000, "{a:?}");
}

#[test]
fn replay_from_a_journal_matches_replay_from_memory() {
    let commands = generate(3, 20_000);
    let mut journal = JournalWriter::new(Vec::new()).unwrap();
    for cmd in &commands {
        journal.append(cmd).unwrap();
    }
    let bytes = journal.finish().unwrap();
    let decoded = read_journal(&bytes).unwrap();
    assert_eq!(decoded.torn_tail, None);
    assert_eq!(decoded.commands, commands);
    assert_eq!(event_bytes(&decoded.commands), event_bytes(&commands));
}

#[test]
fn different_flow_different_digest() {
    let (a, _) = event_bytes(&generate(1, 5_000));
    let (b, _) = event_bytes(&generate(2, 5_000));
    assert_ne!(a.digest, b.digest);
    // Dropping a single command changes the digest too.
    let commands = generate(1, 5_000);
    let mut fewer = commands.clone();
    fewer.remove(2_500);
    assert_ne!(
        event_bytes(&commands).0.digest,
        event_bytes(&fewer).0.digest
    );
}

/// The generated flow must stay valid for the ledger and invariants: it's the workload
/// for every benchmark and differential test after this.
#[test]
fn generated_flow_conserves_quantity() {
    for seed in [1, 2, 3] {
        let mut book = RefBook::new();
        let mut ledger = Ledger::new();
        let mut events = Vec::new();
        for (n, cmd) in generate(seed, 30_000).iter().enumerate() {
            events.clear();
            book.apply(cmd, &mut events);
            let fail = |e: String| panic!("seed {seed}, command #{n} `{cmd}`: {e}");
            book.check_invariants().unwrap_or_else(fail);
            ledger.observe(cmd, &events, &book).unwrap_or_else(fail);
        }
    }
}

/// A pinned digest for a fixed workload. If matching behaviour changes on purpose, this
/// changes, and DESIGN.md must say why. If it changes by accident, that's a bug.
#[test]
fn golden_digest() {
    let (stats, _) = event_bytes(&generate(1, 20_000));
    assert_eq!(
        (stats.commands, stats.events, stats.trades, stats.rejects),
        (20_000, 28_834, 8_316, 5_519)
    );
    assert_eq!(stats.digest, 0xf0cd_0c4b_e21b_0c27);
}

/// The same, for a flow where a third of the big passive orders are icebergs (D89): it pins
/// the `replenished` event's encoding and every iceberg rule along with it.
#[test]
fn golden_iceberg_digest() {
    let config = GenConfig {
        iceberg_pct: 33,
        ..GenConfig::with_seed(1)
    };
    let commands: Vec<Command> = Generator::new(config).take(20_000).collect();
    let (stats, _) = event_bytes(&commands);
    assert_eq!(
        (stats.commands, stats.events, stats.trades, stats.rejects),
        (20_000, 33_618, 10_659, 5_352)
    );
    assert_eq!(stats.digest, 0x4249_c081_da9a_582d);
}

/// The grouped flow (D67) really attempts self-trades, and every action shows up as the
/// cancels D68 allows it, while the ledger checks each one and quantity is conserved.
#[test]
fn grouped_flow_exercises_every_stp_action() {
    let config = GenConfig {
        stp_groups: 3,
        ..GenConfig::with_seed(4)
    };
    let mut book = RefBook::new();
    let mut ledger = Ledger::new();
    let mut events = Vec::new();
    // [cn, co, cb] x [resting order cancelled, incoming order cancelled].
    let mut seen = [[0; 2]; 3];
    for (n, cmd) in Generator::new(config).take(30_000).enumerate() {
        events.clear();
        book.apply(&cmd, &mut events);
        let fail = |e: String| panic!("command #{n} `{cmd}`: {e}");
        book.check_invariants().unwrap_or_else(fail);
        ledger.observe(&cmd, &events, &book).unwrap_or_else(fail);
        let Some(stp) = cmd.new_order().and_then(|o| o.stp) else {
            continue;
        };
        for e in &events {
            if let Event::SelfTradeCancelled { id, .. } = *e {
                let action = match stp.action {
                    StpAction::CancelNewest => 0,
                    StpAction::CancelOldest => 1,
                    StpAction::CancelBoth => 2,
                };
                seen[action][usize::from(id == cmd.id())] += 1;
            }
        }
    }
    let [cn, co, cb] = seen;
    assert!(
        cn[1] > 100 && co[0] > 100 && cb[0] > 100 && cb[1] > 100,
        "{seen:?}"
    );
}
