//! Deterministic replay end to end: generator -> journal -> replay -> digest.

use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, JournalWriter};
use lob::ledger::Ledger;
use lob::replay::replay;
use lob::{Command, OrderBook, RefBook};

fn generate(seed: u64, n: usize) -> Vec<Command> {
    Generator::new(GenConfig {
        seed,
        ..GenConfig::default()
    })
    .take(n)
    .collect()
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
