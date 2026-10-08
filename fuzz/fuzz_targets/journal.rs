//! Any bytes as a command payload and as a journal file (D65): never a panic.
//! Random bytes almost never carry a valid CRC32, so a whole-file target alone would
//! never reach the payload decoder: the payload is fuzzed directly, and an accepted
//! payload must be the canonical encoding of its command (as in D50).
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::journal::{decode_command, encode_command, read_journal, MAX_PAYLOAD};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(cmd) = decode_command(bytes) {
        let mut buf = [0; MAX_PAYLOAD];
        let len = encode_command(&cmd, &mut buf);
        assert_eq!(&buf[..len], bytes);
    }
    let _ = read_journal(bytes);
});
