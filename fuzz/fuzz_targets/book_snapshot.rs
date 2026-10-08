//! Any bytes as a book snapshot (D81). The fuzzer can't forge a CRC, so the last 4 bytes
//! are replaced with the right one: the input then reaches every check behind the
//! checksum. Never a panic; anything accepted is canonical and restores into both books,
//! which pass their invariants and write the same state back.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::snapshot::Snapshot;
use lob::{FastBook, OrderBook, RefBook};

fuzz_target!(|bytes: &[u8]| {
    let _ = Snapshot::decode(bytes);
    let mut sealed = bytes[..bytes.len().saturating_sub(4)].to_vec();
    let crc = crc32fast::hash(&sealed);
    sealed.extend_from_slice(&crc.to_le_bytes());
    if let Ok(snap) = Snapshot::decode(&sealed) {
        assert_eq!(snap.encode(), sealed);
        let fast = FastBook::from_state(&snap.state).unwrap();
        let reference = RefBook::from_state(&snap.state).unwrap();
        assert_eq!(fast.check_invariants(), Ok(()));
        assert_eq!(reference.check_invariants(), Ok(()));
        assert_eq!(fast.state(), snap.state);
        assert_eq!(reference.state(), snap.state);
    }
});
