//! Any bytes as one ITCH message body, and as a stream (D65): never a panic. A decoded
//! message re-encodes to one that decodes the same. ITCH isn't byte-canonical (D50:
//! a stock's padding can be spelled more than one way), so bytes aren't compared.
//! Types `decode` skips (`Other`) can't be encoded, by design.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::itch::{self, Body, Reader};

fuzz_target!(|bytes: &[u8]| {
    if let Some(msg) = itch::decode(bytes)
        .ok()
        .filter(|m| !matches!(m.body, Body::Other(_)))
    {
        let mut buf = Vec::new();
        itch::encode(&msg, &mut buf);
        assert_eq!(itch::decode(&buf[2..]), Ok(msg));
    }
    let mut reader = Reader::new(bytes);
    while let Ok(Some(_)) = reader.next_message() {}
});
