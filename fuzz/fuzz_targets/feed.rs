//! Any bytes as a feed message and as a snapshot (D65): never a panic, and anything
//! accepted is the canonical encoding of what it decodes to (as in D50).
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::feed;

fuzz_target!(|bytes: &[u8]| {
    if let Ok((msg, len)) = feed::decode(bytes) {
        assert!(len <= bytes.len());
        let mut buf = Vec::new();
        feed::encode(&msg, &mut buf);
        assert_eq!(&buf[..], &bytes[..len]);
    }
    if let Ok(snap) = feed::decode_snapshot(bytes) {
        let mut buf = Vec::new();
        feed::encode_snapshot(&snap, &mut buf);
        assert_eq!(&buf[..], bytes);
    }
});
