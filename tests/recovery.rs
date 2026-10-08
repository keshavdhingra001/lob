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
        .map(|_| common::random_command(&mut rng, &mut next_id, 1, 2, true))
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
    // The engine's writer knows its last record; a writer over the same prefix does too.
    let mut w = JournalWriter::new(Vec::new()).unwrap();
    for cmd in &commands[..k] {
        w.append(cmd).unwrap();
    }
    assert_eq!(w.position(), starts[k]);
    Snapshot {
        state: book.state(),
        stats,
        journal_offset: starts[k],
        last_record: w.last_record(),
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
    // Recovery keeps exactly the records wholly inside the cut, and cuts the rest.
    let complete = starts.iter().filter(|&&s| s <= bytes.len() as u64).count() - 1;
    if kept != complete || r.journal_end != starts[kept] {
        return Err(format!(
            "kept {kept} records (want {complete}), ends at {}",
            r.journal_end
        ));
    }
    if r.last_record.map(|l| l.start) != kept.checked_sub(1).map(|i| starts[i]) {
        return Err(format!("last record {:?} after {kept}", r.last_record));
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
            5..=7 => 25,
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
    assert!(r.from_snapshot && r.bad_snapshot.is_none());
    assert_eq!((r.replayed, r.recorder.stats()), (20, want));
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
    assert!(!r.from_snapshot && r.bad_snapshot.is_some());
    assert_eq!((r.replayed, r.recorder.stats()), (60, want));

    // No snapshot at all.
    fs::remove_file(&spath).unwrap();
    let r = recover_files::<RefBook>(&jpath, &spath, CONFIG, |_| {}).unwrap();
    assert_eq!(r.recorder.stats(), want);
}

/// Two journals whose records all have one size: a snapshot of one lands on a record
/// boundary of the other. Both start with `accepted 1..=n`, so their event streams agree
/// too: only the book tells them apart.
fn same_shape(side: &str, price: u32, n: u64) -> Vec<Command> {
    (1..=n)
        .map(|i| format!("limit {i} {side} 1 {price}").parse().unwrap())
        .collect()
}

#[test]
fn a_snapshot_of_another_journal_is_set_aside() {
    let a = same_shape("buy", 10, 100);
    let b = same_shape("sell", 11, 200);
    let (a_bytes, a_starts) = journal(&a);
    let (b_bytes, _) = journal(&b);
    let stale = snapshot_after(&a, &a_starts, 100);
    assert!(stale.journal_offset < b_bytes.len() as u64);

    let mut truth = RefBook::with_config(CONFIG);
    let want = replay(&mut truth, &b, |_| {});
    let r = recover::<FastBook>(&b_bytes, Some(&stale), CONFIG, |_| {}).unwrap();
    assert!(!r.from_snapshot);
    assert_eq!(
        r.bad_snapshot,
        Some(lob::snapshot::SnapshotError::WrongJournal)
    );
    assert_eq!((r.recorder.stats(), r.book.state()), (want, truth.state()));

    // Past the end of a shorter journal, with the named record's header still there but
    // carrying another checksum: another journal, set aside.
    let (b100, _) = journal(&same_shape("sell", 11, 100));
    let torn = &b100[..b100.len() - 5];
    assert!(torn.len() < stale.journal_offset as usize);
    let r = recover::<RefBook>(torn, Some(&stale), CONFIG, |_| {}).unwrap();
    assert_eq!(
        r.bad_snapshot,
        Some(lob::snapshot::SnapshotError::WrongJournal)
    );
    assert_eq!((r.recorder.stats().commands, r.truncated), (99, 28));
    // Past the end, with nothing of the named record left: it can't be told apart from a
    // journal that lost its tail, so recovery refuses (D79) rather than guess.
    let (b50, _) = journal(&same_shape("sell", 11, 50));
    assert!(matches!(
        recover::<RefBook>(&b50, Some(&stale), CONFIG, |_| {}),
        Err(RecoveryError::SnapshotPastJournal { .. })
    ));
    // The snapshot's own last record, damaged in place (it's the journal's last record):
    // the journal lost a command the snapshot covers, so recovery refuses.
    let mut damaged = a_bytes.clone();
    let n = damaged.len();
    damaged[n - 1] ^= 1;
    assert!(matches!(
        recover::<RefBook>(&damaged, Some(&stale), CONFIG, |_| {}),
        Err(RecoveryError::SnapshotPastJournal { .. })
    ));
    // A journal that holds the snapshot's own prefix still uses it.
    assert!(
        recover::<RefBook>(&a_bytes, Some(&stale), CONFIG, |_| {})
            .unwrap()
            .from_snapshot
    );
}

#[test]
fn a_new_journal_removes_a_stale_snapshot() {
    use lob::engine::{Engine, Sync};
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("stale-snapshot");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (ja, jb, s) = (dir.join("ja"), dir.join("jb"), dir.join("s"));
    let a = same_shape("buy", 10, 100);
    let b = same_shape("sell", 11, 200);

    let (mut engine, _) =
        Engine::<FastBook>::open(&ja, &s, CONFIG, Sync::None, 50, |_| {}).unwrap();
    engine.process(&a[..60], |_| {}).unwrap();
    engine.close().unwrap();
    // A restart with nothing new, then a snapshot straight away: it must still name the
    // journal's last record, which the writer only knows from recovery.
    let (mut engine, _) =
        Engine::<FastBook>::open(&ja, &s, CONFIG, Sync::None, 50, |_| {}).unwrap();
    engine.snapshot().unwrap();
    drop(engine);
    let (mut engine, opened) =
        Engine::<FastBook>::open(&ja, &s, CONFIG, Sync::None, 50, |_| {}).unwrap();
    assert!(opened.from_snapshot && opened.bad_snapshot.is_none());
    engine.process(&a[60..], |_| {}).unwrap();
    engine.snapshot().unwrap();
    engine.close().unwrap();
    let r = recover_files::<RefBook>(&ja, &s, CONFIG, |_| {}).unwrap();
    assert!(
        r.from_snapshot && r.replayed == 0,
        "the snapshot is used, not set aside"
    );
    assert!(s.exists());

    // A new journal at another path, the same snapshot path, and a crash before the
    // first snapshot of its own.
    let (mut engine, opened) =
        Engine::<FastBook>::open(&jb, &s, CONFIG, Sync::None, 0, |_| {}).unwrap();
    assert!(opened.fresh && !s.exists(), "the stale snapshot is gone");
    engine.process(&b, |_| {}).unwrap();
    drop(engine);
    let mut truth = RefBook::with_config(CONFIG);
    replay(&mut truth, &b, |_| {});
    let r = recover_files::<RefBook>(&jb, &s, CONFIG, |_| {}).unwrap();
    assert_eq!(r.book.state(), truth.state());
}
