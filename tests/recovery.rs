//! Crash recovery (D79, D81): a crash can stop the engine after any byte of the journal
//! and after any snapshot. For every such point, recovering and then running the rest of
//! the session must give the uninterrupted session's events, book and digest.

mod common;

use std::fs;

use lob::journal::JournalWriter;
use lob::recovery::{recover, recover_files, write_snapshot, RecoveryError};
use lob::replay::{replay, ReplayStats};
use lob::rng::Rng;
use lob::snapshot::{BookState, Snapshot};
use lob::{BookConfig, Command, FastBook, OrderBook, RefBook};

const CONFIG: BookConfig = BookConfig {
    tick_size: 1,
    max_qty: 12,
};

fn session(seed: u64, n: usize) -> Vec<Command> {
    let mut rng = Rng::new(seed);
    let mut next_id = 1;
    (0..n)
        .map(|_| common::random_command(&mut rng, &mut next_id, 1, 2))
        .collect()
}

/// The journal's bytes and where each record starts (plus the end).
fn journal(commands: &[Command]) -> (Vec<u8>, Vec<u64>) {
    let mut w = JournalWriter::new(Vec::new()).unwrap();
    let mut starts = vec![w.position()];
    for cmd in commands {
        w.append(cmd).unwrap();
        starts.push(w.position());
    }
    (w.finish().unwrap(), starts)
}

/// The snapshot an engine would take after `k` commands.
fn snapshot_after(commands: &[Command], starts: &[u64], k: usize) -> Snapshot {
    let mut book = RefBook::with_config(CONFIG);
    let stats = replay(&mut book, &commands[..k], |_| {});
    Snapshot {
        state: book.state(),
        stats,
        journal_offset: starts[k],
    }
}

/// The uninterrupted session: its stats, final book and event bytes.
struct Want {
    stats: ReplayStats,
    state: BookState,
    events: Vec<u8>,
}

impl Want {
    fn new(commands: &[Command]) -> Self {
        let mut book = RefBook::with_config(CONFIG);
        let mut events = Vec::new();
        let stats = replay(&mut book, commands, |b| events.extend_from_slice(b));
        Want {
            stats,
            state: book.state(),
            events,
        }
    }
}

/// Recover from `bytes`, run the commands the journal lost, and compare with `want`.
fn check<B: OrderBook>(
    commands: &[Command],
    want: &Want,
    bytes: &[u8],
    snap: Option<&Snapshot>,
    starts: &[u64],
) -> Result<(), String> {
    let mut events = Vec::new();
    let mut r = recover::<B>(bytes, snap, CONFIG, |b| events.extend_from_slice(b))
        .map_err(|e| e.to_string())?;
    let kept = r.recorder.stats().commands as usize;
    // Recovery keeps exactly the complete records and cuts the rest.
    if starts[kept] != r.journal_end || r.truncated != bytes.len() as u64 - r.journal_end {
        return Err(format!("kept {kept} records but ends at {}", r.journal_end));
    }
    let skipped = snap.map_or(0, |s| s.stats.events) as usize;
    for cmd in &commands[kept..] {
        r.recorder
            .apply(&mut r.book, cmd, &mut |b| events.extend_from_slice(b));
    }
    r.recorder.flush(&mut |b| events.extend_from_slice(b));
    // Events after the snapshot are the uninterrupted run's, byte for byte.
    let tail_start = event_byte_offset(&want.events, skipped);
    if events != want.events[tail_start..] {
        return Err("event bytes differ".into());
    }
    if r.recorder.stats() != want.stats || r.book.state() != want.state {
        return Err(format!("{:?} != {:?}", r.recorder.stats(), want.stats));
    }
    Ok(())
}

/// Byte offset of event number `n + 1` in an encoded event stream.
fn event_byte_offset(stream: &[u8], n: usize) -> usize {
    let mut pos = 0;
    for _ in 0..n {
        pos += match stream[pos + 8] {
            1 => 17,
            2 => 18,
            3 => 33,
            4 => 42,
            5 | 6 => 25,
            t => panic!("bad tag {t}"),
        };
    }
    pos
}

#[test]
fn every_crash_point_recovers_to_the_uninterrupted_session() {
    for seed in 1..=40 {
        let commands = session(seed, 200);
        let (bytes, starts) = journal(&commands);
        let want = Want::new(&commands);
        // Snapshot points: none, the start, a few in the middle, the end.
        let mut rng = Rng::new(seed);
        let mut points: Vec<Option<usize>> = vec![None, Some(0), Some(commands.len())];
        points.extend((0..4).map(|_| Some(rng.below(commands.len() as u64) as usize)));
        for k in points {
            let snap = k.map(|k| snapshot_after(&commands, &starts, k));
            let first = snap.as_ref().map_or(8, |s| s.journal_offset as usize);
            // Cuts at or after the snapshot's offset: every record boundary, and one byte
            // inside every record (a crash mid-append).
            let mut cuts = vec![bytes.len()];
            for w in starts.windows(2).filter(|w| w[0] as usize >= first) {
                cuts.push(w[0] as usize);
                cuts.push((w[0] + 1 + rng.below(w[1] - w[0] - 1)) as usize);
            }
            for cut in cuts {
                for result in [
                    check::<RefBook>(&commands, &want, &bytes[..cut], snap.as_ref(), &starts),
                    check::<FastBook>(&commands, &want, &bytes[..cut], snap.as_ref(), &starts),
                ] {
                    if let Err(e) = result {
                        panic!("seed {seed}, snapshot after {k:?}, cut at {cut}: {e}");
                    }
                }
            }
        }
    }
}

#[test]
fn a_snapshot_past_the_journal_is_refused() {
    let commands = session(7, 50);
    let (bytes, starts) = journal(&commands);
    let snap = snapshot_after(&commands, &starts, 30);
    let cut = starts[30] as usize - 1;
    assert!(matches!(
        recover::<FastBook>(&bytes[..cut], Some(&snap), CONFIG, |_| {}),
        Err(RecoveryError::SnapshotPastJournal { .. })
    ));
    let other = BookConfig {
        tick_size: 2,
        ..CONFIG
    };
    assert!(matches!(
        recover::<FastBook>(&bytes, Some(&snap), other, |_| {}),
        Err(RecoveryError::ConfigMismatch)
    ));
}

#[test]
fn damage_before_the_last_record_is_refused_even_after_a_snapshot() {
    let commands = session(8, 50);
    let (mut bytes, starts) = journal(&commands);
    let snap = snapshot_after(&commands, &starts, 20);
    bytes[starts[30] as usize + 7] ^= 1;
    assert!(matches!(
        recover::<RefBook>(&bytes, Some(&snap), CONFIG, |_| {}),
        Err(RecoveryError::Journal(lob::journal::JournalError::Corrupt(
            _
        )))
    ));
    // Damage the snapshot already covers is never read: the journal before the offset is
    // history, and the snapshot holds its result.
    let (mut bytes, _) = journal(&commands);
    bytes[starts[10] as usize + 7] ^= 1;
    assert!(recover::<RefBook>(&bytes, Some(&snap), CONFIG, |_| {}).is_ok());
}

#[test]
fn files_recover_truncate_the_torn_tail_and_survive_a_bad_snapshot() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("recovery-files");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (jpath, spath) = (dir.join("j"), dir.join("s"));
    let commands = session(9, 80);
    let (bytes, starts) = journal(&commands);
    let want = replay(&mut RefBook::with_config(CONFIG), &commands[..60], |_| {});

    // A crash mid-append: half of record 61 on disk.
    let cut = (starts[60] + starts[61]) / 2;
    fs::write(&jpath, &bytes[..cut as usize]).unwrap();
    write_snapshot(&spath, &snapshot_after(&commands, &starts, 40)).unwrap();
    let r = recover_files::<FastBook>(&jpath, &spath, CONFIG, |_| {}).unwrap();
    assert!(r.recovered.from_snapshot && r.bad_snapshot.is_none());
    assert_eq!(
        (r.recovered.replayed, r.recovered.recorder.stats()),
        (20, want)
    );
    assert_eq!(
        fs::metadata(&jpath).unwrap().len(),
        starts[60],
        "torn tail cut off"
    );
    assert!(!dir.join("s.tmp").exists());

    // A damaged snapshot: recovery starts from zero and gets the same answer.
    let mut s = fs::read(&spath).unwrap();
    s[20] ^= 1;
    fs::write(&spath, s).unwrap();
    let r = recover_files::<RefBook>(&jpath, &spath, CONFIG, |_| {}).unwrap();
    assert!(!r.recovered.from_snapshot && r.bad_snapshot.is_some());
    assert_eq!(
        (r.recovered.replayed, r.recovered.recorder.stats()),
        (60, want)
    );

    // No snapshot at all.
    fs::remove_file(&spath).unwrap();
    let r = recover_files::<RefBook>(&jpath, &spath, CONFIG, |_| {}).unwrap();
    assert_eq!(r.recovered.recorder.stats(), want);
}
