//! Any bytes as a journal file (D65): never a panic, and whatever is read back,
//! written out again, reads back the same with no torn tail.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::journal::{read_journal, JournalWriter};

fuzz_target!(|bytes: &[u8]| {
    let Ok(journal) = read_journal(bytes) else {
        return;
    };
    let mut out = JournalWriter::new(Vec::new()).unwrap();
    for cmd in &journal.commands {
        out.append(cmd).unwrap();
    }
    let again = read_journal(&out.finish().unwrap()).unwrap();
    assert_eq!(again.commands, journal.commands);
    assert_eq!(again.torn_tail, None);
});
