//! `lob`: the REPL (no arguments), or the replay tools.
//!
//! ```text
//! lob                                   interactive REPL on the reference book
//! lob gen <seed> <count> <journal>      write <count> generated commands to a journal
//! lob replay <journal> [events-file]    replay a journal; print stats and the digest
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, JournalWriter};
use lob::replay::{replay, replay_timed};
use lob::scenario::run_line;
use lob::RefBook;

const HELP: &str = "\
commands:
  limit  <id> <buy|sell> <qty> <price> [gtc|ioc|fok|post]   prices are integer ticks
  market <id> <buy|sell> <qty>
  modify <id> <qty> <price>                                  qty = new open quantity
  cancel <id>
  book                                                       asks above bids, highest first
  config <tick_size> <max_qty>                               start over with these rules
  help | quit";

const USAGE: &str = "\
usage:
  lob                                   interactive REPL
  lob gen <seed> <count> <journal>      write generated commands to a journal file
  lob replay <journal> [events-file]    replay a journal, print stats and digest";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        [] => repl().map_err(|e| e.to_string()),
        ["gen", seed, count, path] => gen(seed, count, path),
        ["replay", path] => replay_file(path, None),
        ["replay", path, events] => replay_file(path, Some(events)),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn gen(seed: &str, count: &str, path: &str) -> Result<(), String> {
    let seed: u64 = seed.parse().map_err(|_| format!("bad seed `{seed}`"))?;
    let count: usize = count.parse().map_err(|_| format!("bad count `{count}`"))?;
    let file = File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut journal = JournalWriter::new(BufWriter::new(file)).map_err(|e| e.to_string())?;
    let config = GenConfig {
        seed,
        ..GenConfig::default()
    };
    for cmd in Generator::new(config).take(count) {
        journal.append(&cmd).map_err(|e| e.to_string())?;
    }
    journal.finish().map_err(|e| e.to_string())?;
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    println!("wrote {count} commands ({size} bytes) to {path}");
    Ok(())
}

fn replay_file(path: &str, events_path: Option<&str>) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let journal = read_journal(&bytes).map_err(|e| format!("{path}: {e}"))?;
    if let Some(at) = journal.torn_tail {
        eprintln!("warning: torn tail at byte {at}; replaying the complete records before it");
    }
    let commands = &journal.commands;
    let (stats, elapsed) = replay_timed(&mut RefBook::new(), commands);
    if let Some(events_path) = events_path {
        let file = File::create(events_path).map_err(|e| format!("{events_path}: {e}"))?;
        let mut out = BufWriter::new(file);
        let mut failed = None;
        let written = replay(&mut RefBook::new(), commands, |chunk| {
            if failed.is_none() {
                failed = out.write_all(chunk).err();
            }
        });
        if let Some(e) = failed.or_else(|| out.flush().err()) {
            return Err(format!("{events_path}: {e}"));
        }
        assert_eq!(written, stats, "replay is deterministic");
    }
    println!(
        "commands {}  events {}  trades {}  rejects {}",
        stats.commands, stats.events, stats.trades, stats.rejects
    );
    println!("digest   {:016x}", stats.digest);
    let secs = elapsed.as_secs_f64();
    println!(
        "time     {:.3} s  ({:.0} commands/s, reference book, includes encoding + hashing)",
        secs,
        stats.commands as f64 / secs
    );
    Ok(())
}

fn repl() -> io::Result<()> {
    println!("lob reference book. Type `help`.");
    let mut book = RefBook::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("> ");
        stdout.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        match line.trim() {
            "help" => println!("{HELP}"),
            "quit" | "exit" => return Ok(()),
            input => {
                let mut out = String::new();
                match run_line(&mut book, input, &mut out) {
                    // Output lines start with "> " for scenario files; drop it here.
                    Ok(()) => out.lines().for_each(|l| println!("{}", &l[2..])),
                    Err(e) => println!("error: {e}"),
                }
            }
        }
    }
}
