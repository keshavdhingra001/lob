//! Any string as a text command (D6, D65): never a panic, and what parses prints back
//! to text that parses to the same command.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lob::Command;

fuzz_target!(|s: &str| {
    if let Ok(cmd) = s.parse::<Command>() {
        assert_eq!(cmd.to_string().parse::<Command>(), Ok(cmd));
    }
});
