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

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::book::{BookConfig, OrderBook};
use crate::journal::{read_journal_from, JournalError};
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
    /// Records replayed after the snapshot (all of them without one).
    pub replayed: u64,
    /// Whether a snapshot was used.
    pub from_snapshot: bool,
    /// Bytes of torn tail cut off the journal.
    pub truncated: u64,
}

/// Recover from journal bytes and an optional snapshot. Events replayed after the snapshot
/// go to `sink`. `config` is the engine's; a snapshot taken under other rules is refused.
pub fn recover<B: OrderBook>(
    journal: &[u8],
    snapshot: Option<&Snapshot>,
    config: BookConfig,
    mut sink: impl FnMut(&[u8]),
) -> Result<Recovered<B>, RecoveryError> {
    let (mut book, mut recorder, offset) = match snapshot {
        Some(snap) => {
            if snap.state.config != config {
                return Err(RecoveryError::ConfigMismatch);
            }
            if snap.journal_offset > journal.len() as u64 {
                return Err(RecoveryError::SnapshotPastJournal {
                    offset: snap.journal_offset,
                    len: journal.len() as u64,
                });
            }
            let book = B::from_state(&snap.state).expect("decode validated the state");
            (book, Recorder::resume(snap.stats), snap.journal_offset)
        }
        None => (B::with_config(config), Recorder::default(), 8),
    };
    let rest = read_journal_from(journal, offset)?;
    for cmd in &rest.commands {
        recorder.apply(&mut book, cmd, &mut sink);
    }
    recorder.flush(&mut sink);
    let journal_end = rest.torn_tail.unwrap_or(journal.len() as u64);
    Ok(Recovered {
        book,
        recorder,
        journal_end,
        replayed: rest.commands.len() as u64,
        from_snapshot: snapshot.is_some(),
        truncated: journal.len() as u64 - journal_end,
    })
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

/// What `recover_files` did, for the CLI to report.
pub struct FileRecovery<B> {
    pub recovered: Recovered<B>,
    /// Set when a snapshot file existed but was unusable, so recovery started from zero.
    pub bad_snapshot: Option<SnapshotError>,
}

/// Recover from files: read both, recover, and cut any torn tail off the journal file
/// (made durable before returning, D79).
pub fn recover_files<B: OrderBook>(
    journal: &Path,
    snapshot: &Path,
    config: BookConfig,
    sink: impl FnMut(&[u8]),
) -> Result<FileRecovery<B>, RecoveryError> {
    let (snap, bad_snapshot) = match read_snapshot(snapshot)? {
        Some(Ok(snap)) => (Some(snap), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let bytes = fs::read(journal)?;
    let recovered = recover(&bytes, snap.as_ref(), config, sink)?;
    if recovered.truncated > 0 {
        let file = OpenOptions::new().write(true).open(journal)?;
        file.set_len(recovered.journal_end)?;
        file.sync_all()?;
    }
    Ok(FileRecovery {
        recovered,
        bad_snapshot,
    })
}
