//! The command journal (D15): a binary recording of the engine's input.
//!
//! Because the engine is deterministic (D4), the journal alone reproduces every event
//! and the final book. Layout:
//!
//! ```text
//! header:  "LOBJ" | version u32
//! record:  crc32 u32 | len u16 | payload (len bytes)      crc covers len + payload
//! payload: tag u8, then fixed-width little-endian fields:
//!   1 limit   id u64 | side u8 | qty u64 | price i64 | tif u8     (27 bytes)
//!   2 market  id u64 | side u8 | qty u64                         (18 bytes)
//!   3 modify  id u64 | qty u64 | price i64                       (25 bytes)
//!   4 cancel  id u64                                             ( 9 bytes)
//!   5 limit with an STP group: limit's fields | group u16 | action u8   (30 bytes)
//!   6 market with an STP group: market's fields | group u16 | action u8 (21 bytes)
//! ```
//!
//! Version 2 added tags 5 and 6 (D73). Ungrouped orders still use tags 1 and 2, so a
//! version 1 file is a valid version 2 file and reads unchanged.

use std::io::{self, Write};

use thiserror::Error;

use crate::command::{Command, Stp, StpAction, TimeInForce};
use crate::types::{OrderId, Price, Qty, Side};

pub const MAGIC: &[u8; 4] = b"LOBJ";
pub const VERSION: u32 = 2;
pub const HEADER_LEN: usize = 8;
/// crc32 + len.
const RECORD_HEADER_LEN: usize = 6;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JournalError {
    #[error("not a journal (bad magic)")]
    BadMagic,
    #[error("unsupported journal version {0}")]
    UnsupportedVersion(u32),
    /// A record failed its checksum and more data follows it, so this isn't a torn
    /// write from a crash: the file is damaged (D16).
    #[error("corrupt record at offset {0}")]
    Corrupt(u64),
    /// The checksum passed but the payload doesn't decode: a bug or a format mismatch.
    /// A start offset inside the header or past the end of the file.
    #[error("offset {0} is outside the journal's records")]
    OffsetOutOfRange(u64),
    #[error("invalid record at offset {offset}: {reason}")]
    InvalidRecord { offset: u64, reason: &'static str },
}

/// A decoded journal.
#[derive(Debug, PartialEq, Eq)]
pub struct Journal {
    pub commands: Vec<Command>,
    /// Set when the file ends in an incomplete or damaged last record: the offset
    /// where the valid data ends. Everything before it was decoded.
    pub torn_tail: Option<u64>,
}

/// Where a record starts and the CRC it carries: enough to recognise it again. A snapshot
/// names the last record it covers this way, so recovery can tell its journal from
/// another one (D82).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRef {
    pub start: u64,
    pub crc: u32,
}

/// The intact record starting at `start` in `bytes`: its reference and where it ends.
/// `None` if it's cut short or fails its checksum.
pub fn record_at(bytes: &[u8], start: u64) -> Option<(RecordRef, u64)> {
    let rest = bytes.get(usize::try_from(start).ok()?..)?;
    let header = rest.get(..RECORD_HEADER_LEN)?;
    let crc = u32::from_le_bytes(header[..4].try_into().unwrap());
    let end = RECORD_HEADER_LEN + u16::from_le_bytes([header[4], header[5]]) as usize;
    let covered = rest.get(4..end)?;
    (crc32fast::hash(covered) == crc).then_some((RecordRef { start, crc }, start + end as u64))
}

pub struct JournalWriter<W: Write> {
    out: W,
    buf: Vec<u8>,
    /// Bytes in the file so far, header included: where the next record starts.
    len: u64,
    /// The last record written, `None` while the journal has none.
    last: Option<RecordRef>,
}

impl<W: Write> JournalWriter<W> {
    /// Writes the header. Wrap `out` in a `BufWriter` for files.
    pub fn new(mut out: W) -> io::Result<Self> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        Ok(JournalWriter {
            out,
            buf: Vec::with_capacity(64),
            len: HEADER_LEN as u64,
            last: None,
        })
    }

    /// Append to a journal that already holds `len` valid bytes, header included, the
    /// last of them in record `last`. `out` must be positioned at `len` (after a
    /// recovery truncated any torn tail, D79).
    pub fn resume(out: W, len: u64, last: Option<RecordRef>) -> Self {
        JournalWriter {
            out,
            buf: Vec::with_capacity(64),
            len,
            last,
        }
    }

    /// Where the next record will start.
    pub fn position(&self) -> u64 {
        self.len
    }

    /// The last record written (or found on resume).
    pub fn last_record(&self) -> Option<RecordRef> {
        self.last
    }

    /// Flush buffered records to `out`, keeping the writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn get_ref(&self) -> &W {
        &self.out
    }

    pub fn append(&mut self, cmd: &Command) -> io::Result<()> {
        let mut payload = [0u8; 32];
        let len = encode_command(cmd, &mut payload);
        let len_bytes = (len as u16).to_le_bytes();
        let mut crc = crc32fast::Hasher::new();
        crc.update(&len_bytes);
        crc.update(&payload[..len]);
        let crc = crc.finalize();
        self.buf.clear();
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.buf.extend_from_slice(&len_bytes);
        self.buf.extend_from_slice(&payload[..len]);
        self.last = Some(RecordRef {
            start: self.len,
            crc,
        });
        self.len += self.buf.len() as u64;
        self.out.write_all(&self.buf)
    }

    /// Flush and hand back the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Encode `cmd` into `buf`, returning the payload length.
pub fn encode_command(cmd: &Command, buf: &mut [u8; 32]) -> usize {
    let mut w = Cursor { buf, pos: 0 };
    match *cmd {
        Command::Limit {
            id,
            side,
            qty,
            price,
            tif,
            stp,
        } => {
            w.u8(if stp.is_some() { 5 } else { 1 });
            w.u64(id.0);
            w.u8(side_byte(side));
            w.u64(qty.0);
            w.u64(price.0 as u64);
            w.u8(tif_byte(tif));
            w.stp(stp);
        }
        Command::Market { id, side, qty, stp } => {
            w.u8(if stp.is_some() { 6 } else { 2 });
            w.u64(id.0);
            w.u8(side_byte(side));
            w.u64(qty.0);
            w.stp(stp);
        }
        Command::Modify { id, qty, price } => {
            w.u8(3);
            w.u64(id.0);
            w.u64(qty.0);
            w.u64(price.0 as u64);
        }
        Command::Cancel { id } => {
            w.u8(4);
            w.u64(id.0);
        }
    }
    w.pos
}

pub fn decode_command(payload: &[u8]) -> Result<Command, &'static str> {
    let mut r = Reader { buf: payload };
    let tag = r.u8()?;
    let cmd = match tag {
        1 | 5 => Command::Limit {
            id: OrderId(r.u64()?),
            side: byte_side(r.u8()?)?,
            qty: Qty(r.u64()?),
            price: Price(r.u64()? as i64),
            tif: byte_tif(r.u8()?)?,
            stp: if tag == 5 { Some(r.stp()?) } else { None },
        },
        2 | 6 => Command::Market {
            id: OrderId(r.u64()?),
            side: byte_side(r.u8()?)?,
            qty: Qty(r.u64()?),
            stp: if tag == 6 { Some(r.stp()?) } else { None },
        },
        3 => Command::Modify {
            id: OrderId(r.u64()?),
            qty: Qty(r.u64()?),
            price: Price(r.u64()? as i64),
        },
        4 => Command::Cancel {
            id: OrderId(r.u64()?),
        },
        _ => return Err("unknown command tag"),
    };
    if !r.buf.is_empty() {
        return Err("trailing bytes after command");
    }
    Ok(cmd)
}

/// Decode a whole journal. A torn last record is reported in `torn_tail`, not an error;
/// damage anywhere before the last record is an error (D16).
pub fn read_journal(bytes: &[u8]) -> Result<Journal, JournalError> {
    read_journal_from(bytes, HEADER_LEN as u64)
}

/// `read_journal`, decoding only the records from byte `offset` on: where a snapshot
/// says its records end (D75). The header is still checked. `offset` must be a record
/// boundary; anything else almost surely fails a checksum and reads as damage.
pub fn read_journal_from(bytes: &[u8], offset: u64) -> Result<Journal, JournalError> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
        return Err(JournalError::BadMagic);
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if !(1..=VERSION).contains(&version) {
        return Err(JournalError::UnsupportedVersion(version));
    }
    if offset < HEADER_LEN as u64 || offset > bytes.len() as u64 {
        return Err(JournalError::OffsetOutOfRange(offset));
    }
    let mut commands = Vec::new();
    let mut pos = offset as usize;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        let torn = Journal {
            commands: Vec::new(),
            torn_tail: Some(pos as u64),
        };
        if rest.len() < RECORD_HEADER_LEN {
            return Ok(Journal { commands, ..torn });
        }
        let crc = u32::from_le_bytes(rest[..4].try_into().unwrap());
        let len = u16::from_le_bytes(rest[4..6].try_into().unwrap()) as usize;
        let end = RECORD_HEADER_LEN + len;
        if rest.len() < end {
            // The length may itself be garbage, but either way nothing complete follows.
            return Ok(Journal { commands, ..torn });
        }
        if crc32fast::hash(&rest[4..end]) != crc {
            if end == rest.len() {
                return Ok(Journal { commands, ..torn });
            }
            return Err(JournalError::Corrupt(pos as u64));
        }
        let cmd = decode_command(&rest[RECORD_HEADER_LEN..end]).map_err(|reason| {
            JournalError::InvalidRecord {
                offset: pos as u64,
                reason,
            }
        })?;
        commands.push(cmd);
        pos += end;
    }
    Ok(Journal {
        commands,
        torn_tail: None,
    })
}

pub(crate) fn side_byte(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

pub(crate) fn byte_side(b: u8) -> Result<Side, &'static str> {
    match b {
        0 => Ok(Side::Buy),
        1 => Ok(Side::Sell),
        _ => Err("bad side byte"),
    }
}

pub(crate) fn action_byte(action: StpAction) -> u8 {
    match action {
        StpAction::CancelNewest => 1,
        StpAction::CancelOldest => 2,
        StpAction::CancelBoth => 3,
    }
}

pub(crate) fn byte_action(b: u8) -> Result<StpAction, &'static str> {
    match b {
        1 => Ok(StpAction::CancelNewest),
        2 => Ok(StpAction::CancelOldest),
        3 => Ok(StpAction::CancelBoth),
        _ => Err("bad stp action byte"),
    }
}

fn tif_byte(tif: TimeInForce) -> u8 {
    match tif {
        TimeInForce::Gtc => 0,
        TimeInForce::Ioc => 1,
        TimeInForce::Fok => 2,
        TimeInForce::PostOnly => 3,
    }
}

fn byte_tif(b: u8) -> Result<TimeInForce, &'static str> {
    match b {
        0 => Ok(TimeInForce::Gtc),
        1 => Ok(TimeInForce::Ioc),
        2 => Ok(TimeInForce::Fok),
        3 => Ok(TimeInForce::PostOnly),
        _ => Err("bad time-in-force byte"),
    }
}

struct Cursor<'a> {
    buf: &'a mut [u8; 32],
    pos: usize,
}

impl Cursor<'_> {
    fn u8(&mut self, v: u8) {
        self.buf[self.pos] = v;
        self.pos += 1;
    }

    fn u64(&mut self, v: u64) {
        self.buf[self.pos..self.pos + 8].copy_from_slice(&v.to_le_bytes());
        self.pos += 8;
    }

    /// Nothing for an ungrouped order: its tag already says so.
    fn stp(&mut self, stp: Option<Stp>) {
        if let Some(Stp { group, action }) = stp {
            self.buf[self.pos..self.pos + 2].copy_from_slice(&group.get().to_le_bytes());
            self.pos += 2;
            self.u8(action_byte(action));
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, &'static str> {
        let (&b, rest) = self.buf.split_first().ok_or("record too short")?;
        self.buf = rest;
        Ok(b)
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        if self.buf.len() < 8 {
            return Err("record too short");
        }
        let (bytes, rest) = self.buf.split_at(8);
        self.buf = rest;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn stp(&mut self) -> Result<Stp, &'static str> {
        let group = u16::from_le_bytes([self.u8()?, self.u8()?]);
        Ok(Stp {
            // Group 0 would be a second spelling of "no group" (D73).
            group: group.try_into().map_err(|_| "stp group 0")?,
            action: byte_action(self.u8()?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Command> {
        [
            "limit 1 buy 100 10025",
            "limit 2 sell 7 -40 ioc",
            "limit 3 sell 7 10030 fok",
            "limit 4 buy 1 9 post",
            "market 5 sell 18446744073709551615",
            "modify 1 50 -9223372036854775808",
            "cancel 18446744073709551615",
            "limit 6 buy 3 100 fok g=65535 stp=cn",
            "market 7 sell 2 g=1 stp=cb",
        ]
        .iter()
        .map(|l| l.parse().unwrap())
        .collect()
    }

    fn write(cmds: &[Command]) -> Vec<u8> {
        let mut w = JournalWriter::new(Vec::new()).unwrap();
        for cmd in cmds {
            w.append(cmd).unwrap();
        }
        w.finish().unwrap()
    }

    /// Byte offsets where each record starts, plus the end of the file.
    fn boundaries(bytes: &[u8]) -> Vec<usize> {
        let mut out = vec![HEADER_LEN];
        let mut pos = HEADER_LEN;
        while pos < bytes.len() {
            let len = u16::from_le_bytes(bytes[pos + 4..pos + 6].try_into().unwrap()) as usize;
            pos += RECORD_HEADER_LEN + len;
            out.push(pos);
        }
        out
    }

    #[test]
    fn round_trips_every_command_kind_and_extreme_values() {
        let cmds = sample();
        let journal = read_journal(&write(&cmds)).unwrap();
        assert_eq!(journal.commands, cmds);
        assert_eq!(journal.torn_tail, None);
    }

    #[test]
    fn payload_sizes_match_the_documented_layout() {
        let mut buf = [0u8; 32];
        let sizes: Vec<usize> = sample()
            .iter()
            .map(|c| encode_command(c, &mut buf))
            .collect();
        assert_eq!(sizes, [27, 27, 27, 27, 18, 25, 9, 30, 21]);
    }

    #[test]
    fn empty_journal_is_valid() {
        assert_eq!(
            read_journal(&write(&[])).unwrap(),
            Journal {
                commands: vec![],
                torn_tail: None,
            }
        );
    }

    #[test]
    fn rejects_bad_header() {
        let valid = write(&sample());
        for offset in [0, 7, valid.len() as u64 + 1] {
            assert_eq!(
                read_journal_from(&valid, offset),
                Err(JournalError::OffsetOutOfRange(offset))
            );
        }
        assert_eq!(read_journal(b"LOB"), Err(JournalError::BadMagic));
        assert_eq!(read_journal(b"XXXX\x01\0\0\0"), Err(JournalError::BadMagic));
        assert_eq!(
            read_journal(b"LOBJ\x03\0\0\0"),
            Err(JournalError::UnsupportedVersion(3))
        );
        assert_eq!(
            read_journal(b"LOBJ\0\0\0\0"),
            Err(JournalError::UnsupportedVersion(0))
        );
    }

    #[test]
    fn reads_a_version_1_file() {
        // Version 1 had no STP tags: the same records under the old header (D73).
        let cmds = &sample()[..7];
        let mut bytes = write(cmds);
        bytes[4] = 1;
        assert_eq!(read_journal(&bytes).unwrap().commands, cmds);
    }

    #[test]
    fn every_truncation_keeps_the_complete_records() {
        let cmds = sample();
        let bytes = write(&cmds);
        let bounds = boundaries(&bytes);
        for cut in HEADER_LEN..=bytes.len() {
            let journal = read_journal(&bytes[..cut]).unwrap();
            // Records that end at or before the cut survive; a partial one is torn.
            let complete = bounds.iter().filter(|&&b| b <= cut).count() - 1;
            assert_eq!(journal.commands, cmds[..complete], "cut at {cut}");
            let torn = !bounds.contains(&cut);
            assert_eq!(
                journal.torn_tail,
                torn.then_some(bounds[complete] as u64),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_flipped_bit_in_the_last_record_is_a_torn_tail() {
        let cmds = sample();
        let mut bytes = write(&cmds);
        let last = *boundaries(&bytes).iter().rev().nth(1).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        let journal = read_journal(&bytes).unwrap();
        assert_eq!(journal.commands, cmds[..cmds.len() - 1]);
        assert_eq!(journal.torn_tail, Some(last as u64));
    }

    #[test]
    fn a_flipped_bit_before_the_last_record_is_corruption() {
        let bytes = write(&sample());
        let bounds = boundaries(&bytes);
        // Flip every byte of every record except the last, one at a time.
        for (i, window) in bounds.windows(2).enumerate().take(bounds.len() - 2) {
            for byte in window[0]..window[1] {
                let mut damaged = bytes.clone();
                damaged[byte] ^= 0x10;
                let result = read_journal(&damaged);
                let is_len = (window[0] + 4..window[0] + 6).contains(&byte);
                match result {
                    Err(JournalError::Corrupt(at)) => assert_eq!(at, window[0] as u64),
                    // A damaged length can claim the record runs past EOF. That looks
                    // exactly like a torn write, so it's the one case reported as torn.
                    Ok(journal) if is_len => {
                        assert_eq!(journal.torn_tail, Some(window[0] as u64));
                        assert_eq!(journal.commands.len(), i);
                    }
                    other => panic!("record {i} byte {byte}: {other:?}"),
                }
            }
        }
    }

    /// A record with a correct CRC around an arbitrary payload.
    fn raw_record(payload: &[u8]) -> Vec<u8> {
        let len = (payload.len() as u16).to_le_bytes();
        let mut crc = crc32fast::Hasher::new();
        crc.update(&len);
        crc.update(payload);
        let mut bytes = write(&[]);
        bytes.extend_from_slice(&crc.finalize().to_le_bytes());
        bytes.extend_from_slice(&len);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn a_valid_checksum_over_a_bad_payload_is_invalid() {
        let invalid = |payload: &[u8]| match read_journal(&raw_record(payload)) {
            Err(JournalError::InvalidRecord { offset: 8, reason }) => reason,
            other => panic!("{payload:?}: {other:?}"),
        };
        assert_eq!(invalid(&[9, 0, 0]), "unknown command tag");
        assert_eq!(invalid(&[4, 1, 0, 0]), "record too short");
        // A cancel with one byte too many.
        assert_eq!(
            invalid(&[4, 1, 0, 0, 0, 0, 0, 0, 0, 7]),
            "trailing bytes after command"
        );
        // A limit with side byte 2.
        let mut limit = [0u8; 27];
        limit[0] = 1;
        limit[9] = 2;
        assert_eq!(invalid(&limit), "bad side byte");
        limit[9] = 0;
        limit[26] = 4;
        assert_eq!(invalid(&limit), "bad time-in-force byte");
        // A grouped market order: group 0 is "no group", which has its own tag.
        let mut market = [0u8; 21];
        market[0] = 6;
        market[20] = 1;
        assert_eq!(invalid(&market), "stp group 0");
        market[18] = 1;
        market[20] = 0;
        assert_eq!(invalid(&market), "bad stp action byte");
        assert_eq!(invalid(&market[..20]), "record too short");
    }
}
