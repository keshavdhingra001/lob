//! Crash recovery (D76–D79): the latest snapshot plus the journal after it.
//!
//! The journal is the truth and the snapshot only saves replaying it from the start
//! (D80). Recovery restores the snapshot's book and counters, then replays the records
//! from the snapshot's offset. Because the engine is deterministic (D4), the result is the
//! book, event numbers and digest of a run that never stopped.
//!
//! - A torn last record is what a crash mid-append leaves: it's cut off, so the next
//!   append doesn't land after garbage (D79).
//! - Damage before the last record is not a crash, so recovery refuses (D16).
//! - A missing or damaged snapshot falls back to replaying everything: slower, never wrong.
//! - A snapshot pointing past the end of the journal refuses: it describes commands the
//!   journal lost, so recovering would contradict what was already published.
//! - A snapshot of another journal (its last record isn't in this one) is set aside, and
//!   recovery starts from zero (D82).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::book::{BookConfig, OrderBook};
use crate::journal::{read_journal_from, record_at, JournalError, RecordRef, HEADER_LEN};
use crate::replay::Recorder;
use crate::snapshot::{Snapshot, SnapshotError};

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("snapshot covers the journal up to byte {offset}, but it has only {len} bytes")]
    SnapshotPastJournal { offset: u64, len: u64 },
    #[error("snapshot was taken under different book rules")]
    ConfigMismatch,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A recovered session, ready to take the next command.
pub struct Recovered<B> {
    pub book: B,
    pub recorder: Recorder,
    /// Where the journal's valid records end: the next append goes here.
    pub journal_end: u64,
    /// The last valid record, for the writer that appends after it (D82).
    pub last_record: Option<RecordRef>,
    /// Records replayed after the snapshot (all of them without one).
    pub replayed: u64,
    /// Whether a snapshot was used.
    pub from_snapshot: bool,
    /// Set when a snapshot existed but couldn't be used, so recovery started from zero.
    pub bad_snapshot: Option<SnapshotError>,
    /// Bytes of torn tail cut off the journal.
    pub truncated: u64,
}

/// Recover from journal bytes and an optional snapshot. Events replayed after the snapshot
/// go to `sink`. `config` is the engine's; a snapshot taken under other rules is refused.
///
/// A snapshot whose last record isn't in this journal is set aside, with a warning, and
/// recovery starts from zero (D82): it belongs to another journal. If that record is
/// missing past the end of the journal, or there but cut short or damaged, the journal
/// lost commands the snapshot covers, and recovery refuses (D79).
pub fn recover<B: OrderBook>(
    journal: &[u8],
    snapshot: Option<&Snapshot>,
    config: BookConfig,
    mut sink: impl FnMut(&[u8]),
) -> Result<Recovered<B>, RecoveryError> {
    let mut bad_snapshot = None;
    let mut start = None;
    if let Some(snap) = snapshot {
        if snap.state.config != config {
            return Err(RecoveryError::ConfigMismatch);
        }
        if snap.matches_journal(journal) {
            match B::from_state(&snap.state) {
                Ok(book) => start = Some((book, snap)),
                Err(reason) => bad_snapshot = Some(SnapshotError::Invalid(reason)),
            }
        } else if lost_tail(snap, journal) {
            return Err(RecoveryError::SnapshotPastJournal {
                offset: snap.journal_offset,
                len: journal.len() as u64,
            });
        } else {
            bad_snapshot = Some(SnapshotError::WrongJournal);
        }
    }
    let from_snapshot = start.is_some();
    let (mut book, mut recorder, offset, mut last_record) = match start {
        Some((book, snap)) => (
            book,
            Recorder::resume(snap.stats),
            snap.journal_offset,
            snap.last_record,
        ),
        None => (
            B::with_config(config),
            Recorder::default(),
            HEADER_LEN as u64,
            None,
        ),
    };
    let rest = read_journal_from(journal, offset)?;
    for cmd in &rest.commands {
        recorder.apply(&mut book, cmd, &mut sink);
    }
    recorder.flush(&mut sink);
    let journal_end = rest.torn_tail.unwrap_or(journal.len() as u64);
    // Walk the record headers just replayed to find the last one (no decoding).
    let mut pos = offset;
    while pos < journal_end {
        let (record, end) = record_at(journal, pos).expect("replayed records are intact");
        last_record = Some(record);
        pos = end;
    }
    Ok(Recovered {
        book,
        recorder,
        journal_end,
        last_record,
        replayed: rest.commands.len() as u64,
        from_snapshot,
        bad_snapshot,
        truncated: journal.len() as u64 - journal_end,
    })
}

/// Whether the snapshot's last record is missing because the journal ends before it, or
/// begins with the same checksum but is cut short or damaged: this journal lost what the
/// snapshot covers (D79), as opposed to being another journal (D82).
fn lost_tail(snap: &Snapshot, journal: &[u8]) -> bool {
    let Some(want) = snap.last_record else {
        return false;
    };
    match journal.get(want.start as usize..want.start as usize + 4) {
        None => true,
        Some(crc) => u32::from_le_bytes(crc.try_into().unwrap()) == want.crc,
    }
}

/// Read a snapshot file. `Ok(None)` if there isn't one.
pub fn read_snapshot(path: &Path) -> io::Result<Option<Result<Snapshot, SnapshotError>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(Snapshot::decode(&bytes))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replace the snapshot at `path` atomically (D76): write a temporary file, make it
/// durable, rename it over the old one, then make the rename durable. A crash at any
/// point leaves either the old snapshot or the new one, never half of one.
pub fn write_snapshot(path: &Path, snap: &Snapshot) -> io::Result<()> {
    let mut tmp = PathBuf::from(path);
    tmp.as_mut_os_string().push(".tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&snap.encode())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    sync_dir(path)
}

/// fsync the directory holding `path`, so a create or rename in it survives power loss.
pub fn sync_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

/// Recover from files: read both, recover, and cut any torn tail off the journal file
/// (made durable before returning, D79). An unreadable snapshot is reported in
/// `bad_snapshot`, and recovery starts from zero.
pub fn recover_files<B: OrderBook>(
    journal: &Path,
    snapshot: &Path,
    config: BookConfig,
    sink: impl FnMut(&[u8]),
) -> Result<Recovered<B>, RecoveryError> {
    let (snap, unreadable) = match read_snapshot(snapshot)? {
        Some(Ok(snap)) => (Some(snap), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let bytes = fs::read(journal)?;
    let mut recovered = recover(&bytes, snap.as_ref(), config, sink)?;
    recovered.bad_snapshot = recovered.bad_snapshot.or(unreadable);
    if recovered.truncated > 0 {
        let file = OpenOptions::new().write(true).open(journal)?;
        file.set_len(recovered.journal_end)?;
        file.sync_all()?;
    }
    Ok(recovered)
}
