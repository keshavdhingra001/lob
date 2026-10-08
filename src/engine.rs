//! A live engine with a durable journal (D77, D78): the loop `lob engine` runs.
//!
//! For each batch of commands: append them all to the journal, make the journal durable
//! (one `fdatasync` for the batch: group commit), then apply them and publish their
//! events. **No event is published before its command is durable**, so a recovered engine
//! never contradicts what it already said. Every `snapshot_every` commands the caller
//! takes a snapshot, on this thread, between batches.
//!
//! Opening an existing journal recovers first (D79), so a restart after a crash is the
//! same call as a clean start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::book::{BookConfig, OrderBook};
use crate::command::Command;
use crate::journal::JournalWriter;
use crate::recovery::{recover_files, sync_dir, write_snapshot, RecoveryError};
use crate::replay::{Recorder, ReplayStats};
use crate::snapshot::{Snapshot, SnapshotError};

/// When the journal is made durable (D78).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// Never fsync (except before a snapshot): survives the process dying, not the
    /// machine. Events can be published for commands a power cut then loses.
    None,
    /// fsync each batch before applying it.
    Batch,
}

/// What opening found: a fresh journal, or what recovery did.
#[derive(Debug, Default)]
pub struct Opened {
    pub fresh: bool,
    pub from_snapshot: bool,
    pub replayed: u64,
    pub truncated: u64,
    pub bad_snapshot: Option<SnapshotError>,
}

pub struct Engine<B> {
    pub book: B,
    recorder: Recorder,
    journal: JournalWriter<BufWriter<File>>,
    snapshot_path: PathBuf,
    sync: Sync,
    snapshot_every: u64,
    /// Commands applied since the last snapshot (or since opening).
    since_snapshot: u64,
    /// fsync calls on the journal, for the CLI's report.
    pub syncs: u64,
}

impl<B: OrderBook> Engine<B> {
    /// Open `journal`, recovering from it and `snapshot` if it holds at least a header,
    /// otherwise creating it. Replayed events go to `sink`: they were published before
    /// the crash, and a caller that republishes must start after them.
    pub fn open(
        journal: &Path,
        snapshot: &Path,
        config: BookConfig,
        sync: Sync,
        snapshot_every: u64,
        sink: impl FnMut(&[u8]),
    ) -> Result<(Self, Opened), RecoveryError> {
        let exists = journal.metadata().is_ok_and(|m| m.len() >= 8);
        let (book, recorder, writer, opened) = if exists {
            let r = recover_files::<B>(journal, snapshot, config, sink)?;
            let mut file = OpenOptions::new().write(true).open(journal)?;
            file.seek(SeekFrom::Start(r.journal_end))?;
            let writer = JournalWriter::resume(BufWriter::new(file), r.journal_end, r.last_record);
            let opened = Opened {
                fresh: false,
                from_snapshot: r.from_snapshot,
                replayed: r.replayed,
                truncated: r.truncated,
                bad_snapshot: r.bad_snapshot,
            };
            (r.book, r.recorder, writer, opened)
        } else {
            // A file shorter than the header is a crash during creation: start over. A
            // snapshot left at `snapshot` belongs to some other journal: remove it before
            // anything is written, so no later recovery can apply it to this one (D89).
            match fs::remove_file(snapshot) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            let mut writer = JournalWriter::new(BufWriter::new(File::create(journal)?))?;
            writer.flush()?;
            writer.get_ref().get_ref().sync_all()?;
            sync_dir(journal)?;
            let opened = Opened {
                fresh: true,
                ..Opened::default()
            };
            (B::with_config(config), Recorder::default(), writer, opened)
        };
        let engine = Engine {
            book,
            recorder,
            journal: writer,
            snapshot_path: snapshot.to_path_buf(),
            sync,
            snapshot_every,
            since_snapshot: 0,
            syncs: 0,
        };
        Ok((engine, opened))
    }

    /// Journal `batch`, make it durable (per `Sync`), then apply it. Events go to `sink`.
    pub fn process(&mut self, batch: &[Command], mut sink: impl FnMut(&[u8])) -> io::Result<()> {
        for cmd in batch {
            self.journal.append(cmd)?;
        }
        if self.sync == Sync::Batch {
            self.sync_journal()?;
        }
        for cmd in batch {
            self.recorder.apply(&mut self.book, cmd, &mut sink);
        }
        self.recorder.flush(&mut sink);
        self.since_snapshot += batch.len() as u64;
        Ok(())
    }

    fn sync_journal(&mut self) -> io::Result<()> {
        self.journal.flush()?;
        // Data only: the file's size is metadata, but fdatasync covers a size change
        // that's needed to read the data back.
        self.journal.get_ref().get_ref().sync_data()?;
        self.syncs += 1;
        Ok(())
    }

    pub fn snapshot_due(&self) -> bool {
        self.snapshot_every > 0 && self.since_snapshot >= self.snapshot_every
    }

    /// Write a snapshot of the book now. The journal is made durable first, whatever the
    /// `Sync` mode: a snapshot must never cover commands the journal could still lose.
    pub fn snapshot(&mut self) -> io::Result<()> {
        self.sync_journal()?;
        let snap = Snapshot {
            state: self.book.state(),
            stats: self.recorder.stats(),
            journal_offset: self.journal.position(),
            last_record: self.journal.last_record(),
        };
        write_snapshot(&self.snapshot_path, &snap)?;
        self.since_snapshot = 0;
        Ok(())
    }

    /// Flush and sync the journal: a clean shutdown.
    pub fn close(mut self) -> io::Result<ReplayStats> {
        self.sync_journal()?;
        Ok(self.recorder.stats())
    }

    pub fn stats(&self) -> ReplayStats {
        self.recorder.stats()
    }
}
