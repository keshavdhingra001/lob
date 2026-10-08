//! A real crash (D81): `lob engine` is killed with SIGKILL at several moments, then
//! recovered and restarted. Whatever the moment, `lob recover` must report the digest of
//! exactly the commands the journal kept, and the restarted engine must finish with the
//! digest of a run that never stopped.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::thread::sleep;
use std::time::Duration;

use lob::gen::Generator;
use lob::journal::JournalWriter;
use lob::replay::replay;
use lob::{Command, RefBook};

const LOB: &str = env!("CARGO_BIN_EXE_lob");

fn digest_of(commands: &[Command]) -> String {
    format!(
        "{:016x}",
        replay(&mut RefBook::new(), commands, |_| {}).digest
    )
}

/// Run `lob` to completion and return (commands, digest) from its output.
fn run(args: &[&Path]) -> (usize, String) {
    let out = Process::new(LOB).args(args).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let field = |key: &str| {
        let line = text.lines().find(|l| l.starts_with(key)).unwrap();
        line.split_whitespace().nth(1).unwrap().to_string()
    };
    (field("commands").parse().unwrap(), field("digest"))
}

#[test]
fn sigkill_at_any_moment_recovers_to_the_uninterrupted_digest() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crash");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (input, journal, snap) = (dir.join("in"), dir.join("j"), dir.join("s"));
    let commands: Vec<Command> = Generator::seeded(3).take(60_000).collect();
    let mut w = JournalWriter::new(BufWriter::new(File::create(&input).unwrap())).unwrap();
    for cmd in &commands {
        w.append(cmd).unwrap();
    }
    w.finish().unwrap();

    // Each round restarts the engine (recovering what the last one left) and kills it
    // after a delay; the delays spread the kills over appends, fsyncs and snapshot writes.
    let mut mid_run_kills = 0;
    for delay_ms in [5, 40, 90, 15, 150, 60, 250] {
        let mut child = Process::new(LOB)
            .args([
                "engine".as_ref(),
                input.as_os_str(),
                journal.as_os_str(),
                snap.as_os_str(),
            ])
            .args(["4000", "batch", "8"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        sleep(Duration::from_millis(delay_ms));
        child.kill().unwrap(); // SIGKILL: no destructors, no flush
        child.wait().unwrap();

        // Killed before the journal's header was durable: nothing to recover, and the
        // next engine starts a new journal.
        if fs::metadata(&journal).map_or(true, |m| m.len() < 8) {
            continue;
        }
        let (kept, digest) = run(&["recover".as_ref(), &journal, &snap]);
        assert_eq!(
            digest,
            digest_of(&commands[..kept]),
            "after the {delay_ms} ms kill"
        );
        if kept < commands.len() {
            mid_run_kills += 1;
        }
    }
    assert!(
        mid_run_kills >= 3,
        "only {mid_run_kills} kills landed mid-run"
    );

    let (n, digest) = run(&["engine".as_ref(), &input, &journal, &snap]);
    assert_eq!((n, digest), (commands.len(), digest_of(&commands)));
    assert_eq!(fs::read(&journal).unwrap(), fs::read(&input).unwrap());
}
