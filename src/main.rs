//! `lob`: the REPL (no arguments), or the replay tools.
//!
//! ```text
//! lob                                   interactive REPL on the reference book
//! lob gen <seed> <count> <journal> [max-live]   write <count> generated commands to a journal
//! lob replay <journal> [events-file]    replay a journal; print stats and the digest
//! lob bench <journal>                   replay through both books; compare speed and digests
//! lob gen-queue <orders> <journal>      worst case: one deep queue, cancelled in random order
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, JournalWriter};
use lob::replay::{replay, replay_timed};
use lob::scenario::run_line;
use lob::{Command, FastBook, OrderBook, RefBook};

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
  lob gen <seed> <count> <journal> [max-live]   write generated commands to a journal file
                                        (max-live: orders the generator keeps alive, default 5000)
  lob replay <journal> [events-file]    replay a journal, print stats and digest
  lob bench <journal>                   replay through both books, compare speed and digests
  lob gen-queue <orders> <journal>      worst case for cancel: one deep queue, random cancels";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        [] => repl().map_err(|e| e.to_string()),
        ["gen", seed, count, path] => gen(seed, count, path, None),
        ["gen", seed, count, path, max_live] => gen(seed, count, path, Some(max_live)),
        ["replay", path] => replay_file(path, None),
        ["replay", path, events] => replay_file(path, Some(events)),
        ["bench", path] => bench(path),
        ["gen-queue", n, path] => gen_queue(n, path),
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

fn gen(seed: &str, count: &str, path: &str, max_live: Option<&str>) -> Result<(), String> {
    let seed: u64 = seed.parse().map_err(|_| format!("bad seed `{seed}`"))?;
    let count: usize = count.parse().map_err(|_| format!("bad count `{count}`"))?;
    let max_live = match max_live {
        Some(m) => m.parse().map_err(|_| format!("bad max-live `{m}`"))?,
        None => GenConfig::default().max_live,
    };
    let file = File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut journal = JournalWriter::new(BufWriter::new(file)).map_err(|e| e.to_string())?;
    let config = GenConfig {
        seed,
        max_live,
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

fn gen_queue(n: &str, path: &str) -> Result<(), String> {
    let n: u64 = n.parse().map_err(|_| format!("bad order count `{n}`"))?;
    let file = File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut journal = JournalWriter::new(BufWriter::new(file)).map_err(|e| e.to_string())?;
    for cmd in lob::gen::deep_queue(n, 1) {
        journal.append(&cmd).map_err(|e| e.to_string())?;
    }
    journal.finish().map_err(|e| e.to_string())?;
    println!("wrote {n} queued orders + {n} cancels to {path}");
    Ok(())
}

/// Best of 5 runs per book, each on a fresh book. Whole-session throughput only:
/// per-command latency percentiles are M5.
fn bench(path: &str) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let journal = read_journal(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let commands = &journal.commands;
    let n = commands.len() as f64;

    /// (digest, best full-replay seconds, best apply-only seconds, resting orders at the end)
    fn measure<B: OrderBook>(commands: &[Command]) -> (u64, f64, f64, usize) {
        let (mut replay_best, mut apply_best, mut digest) = (f64::MAX, f64::MAX, 0);
        let mut resting = 0;
        for _ in 0..5 {
            let mut book = B::with_config(Default::default());
            let (stats, elapsed) = replay_timed(&mut book, commands);
            replay_best = replay_best.min(elapsed.as_secs_f64());
            digest = stats.digest;

            let mut book = B::with_config(Default::default());
            let mut events = Vec::with_capacity(64);
            let start = std::time::Instant::now();
            for cmd in commands {
                events.clear();
                book.apply(cmd, &mut events);
            }
            apply_best = apply_best.min(start.elapsed().as_secs_f64());
            resting = [lob::Side::Buy, lob::Side::Sell]
                .iter()
                .flat_map(|&s| book.depth(s, usize::MAX))
                .map(|l| l.orders)
                .sum();
        }
        (digest, replay_best, apply_best, resting)
    }

    let (ref_digest, ref_replay, ref_apply, resting) = measure::<RefBook>(commands);
    let (fast_digest, fast_replay, fast_apply, _) = measure::<FastBook>(commands);
    println!(
        "{} commands, best of 5; {resting} orders resting at the end",
        commands.len()
    );
    println!("                 apply only               full replay (+ encode + hash)");
    let row = |name: &str, apply: f64, replay: f64, digest: u64| {
        println!(
            "{name:<10} {:>8.1} ms {:>6.2} M/s     {:>8.1} ms {:>6.2} M/s   digest {digest:016x}",
            apply * 1e3,
            n / apply / 1e6,
            replay * 1e3,
            n / replay / 1e6
        )
    };
    row("reference", ref_apply, ref_replay, ref_digest);
    row("fast", fast_apply, fast_replay, fast_digest);
    println!(
        "speedup    {:>8.2}x                 {:>8.2}x",
        ref_apply / fast_apply,
        ref_replay / fast_replay
    );
    if ref_digest != fast_digest {
        return Err("digests differ: the books disagree".to_string());
    }
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
