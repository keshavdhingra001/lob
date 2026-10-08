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
use lob::{Command, OrderBook, RefBook};

const LOB: &str = env!("CARGO_BIN_EXE_lob");

/// The event digest and book digest of an uninterrupted run, as the CLI prints them.
fn digest_of(commands: &[Command]) -> (String, String) {
    let mut book = RefBook::new();
    let digest = replay(&mut book, commands, |_| {}).digest;
    (
        format!("{digest:016x}"),
        format!("{:016x}", book.state().digest()),
    )
}

/// Run `lob` to completion and return (commands, (digest, book digest)) from its output.
fn run(args: &[&Path]) -> (usize, (String, String)) {
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
    (
        field("commands").parse().unwrap(),
        (field("digest"), field("book")),
    )
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
    // Snapshots every 50 commands, so most kills land after one and recovery uses it.
    let (mut mid_run_kills, mut durable) = (0, 0);
    for delay_ms in [5, 40, 90, 15, 150, 60, 250] {
        let mut child = Process::new(LOB)
            .args([
                "engine".as_ref(),
                input.as_os_str(),
                journal.as_os_str(),
                snap.as_os_str(),
            ])
            .args(["50", "batch", "8"])
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
        // A restart resumes: what was durable stays durable.
        assert!(kept >= durable, "kept {kept} after {durable} were durable");
        durable = kept;
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
    // And the last snapshot, plus the journal after it, still recovers the whole run.
    let (n, digest) = run(&["recover".as_ref(), &journal, &snap]);
    assert_eq!((n, digest), (commands.len(), digest_of(&commands)));
}

#[test]
fn a_restart_refuses_input_the_journal_does_not_start_with() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crash-input");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (a, b, journal, snap) = (dir.join("a"), dir.join("b"), dir.join("j"), dir.join("s"));
    fs::write(&a, "limit 1 buy 5 100\nlimit 2 buy 5 99\n").unwrap();
    fs::write(
        &b,
        "limit 1 sell 5 100\nlimit 2 sell 5 101\nlimit 3 sell 1 102\n",
    )
    .unwrap();
    assert_eq!(run(&["engine".as_ref(), &a, &journal, &snap]).0, 2);
    let out = Process::new(LOB)
        .arg("engine")
        .args([&b, &journal, &snap])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("a different input?"));
    // The input with more commands after the journaled ones is fine.
    fs::write(
        &a,
        "limit 1 buy 5 100\nlimit 2 buy 5 99\nlimit 3 sell 1 99\n",
    )
    .unwrap();
    assert_eq!(run(&["engine".as_ref(), &a, &journal, &snap]).0, 3);
}
