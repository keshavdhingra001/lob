//! `lob`: the REPL (no arguments), or the replay tools.
//!
//! ```text
//! lob                                   interactive REPL on the reference book
//! lob gen <seed> <count> <journal> [max-live]   write <count> generated commands to a journal
//! lob replay <journal> [events-file]    replay a journal; print stats and the digest
//! lob bench <journal>                   replay through both books; compare speed and digests
//! lob gen-queue <orders> <journal>      worst case: one deep queue, cancelled in random order
//! lob latency <journal> [runs]          per-command latency percentiles for both books
//! lob run <ref|fast|none> <journal> [repeats]   apply only, no timing: for `perf stat` (D29)
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, JournalWriter};
use lob::latency::{measure_interleaved, median_run, table, Report};
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
  lob gen-queue <orders> <journal>      worst case for cancel: one deep queue, random cancels
  lob latency <journal> [runs]          per-command latency percentiles, both books
                                        (runs: default 5; pin it with `taskset -c <cpu>`)
  lob run <ref|fast|none> <journal> [repeats]   apply only, nothing timed or printed per command,
                                        for `perf stat`; `none` only decodes (the baseline)";

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
        ["run", book, path] => run(book, path, "1"),
        ["run", book, path, repeats] => run(book, path, repeats),
        ["latency", path] => latency(path, "5"),
        ["latency", path, runs] => latency(path, runs),
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

/// Per-command latency for both books (D23–D27): a warm-up pass per book, then `runs`
/// timed runs per book on fresh books, alternating, reporting the run with the median p99.
fn latency(path: &str, runs: &str) -> Result<(), String> {
    let runs: usize = match runs.parse() {
        Ok(n) if n > 0 => n,
        _ => return Err(format!("bad run count `{runs}`")),
    };
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let journal = read_journal(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let commands = &journal.commands;
    print!("{}", machine());
    println!(
        "{} commands from {path}; warm-up, then {runs} runs per book, alternating books; showing the run with the median p99\n",
        commands.len()
    );
    // Each run's p99 next to its clock floor (p50 of an empty timed window): a higher
    // floor means the CPU was running slower for that run, so compare runs with similar floors.
    let report = |name: &str, reports: &[Report]| {
        let runs: Vec<String> = reports
            .iter()
            .map(|r| {
                format!(
                    "{}/{}",
                    r.all.value_at_quantile(0.99),
                    r.clock.value_at_quantile(0.5)
                )
            })
            .collect();
        println!("{name} (p99/floor per run: {} ns)", runs.join(" "));
        println!("{}\n", table(median_run(reports)));
    };
    let (reference, fast) = measure_interleaved::<RefBook, FastBook>(commands, runs);
    report("reference", &reference);
    report("fast", &fast);
    Ok(())
}

/// Apply a journal `repeats` times, each on a fresh book, with nothing else in the loop,
/// so `perf stat` counts matching and not timing or printing (D29). `none` decodes the
/// journal and stops: run it too and subtract, to remove the decoding cost.
fn run(book: &str, path: &str, repeats: &str) -> Result<(), String> {
    let repeats: usize = repeats
        .parse()
        .map_err(|_| format!("bad repeat count `{repeats}`"))?;
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let journal = read_journal(&bytes).map_err(|e| format!("{path}: {e}"))?;
    fn apply_all<B: OrderBook>(commands: &[Command], repeats: usize) -> usize {
        let mut events = Vec::with_capacity(64);
        let mut total = 0;
        for _ in 0..repeats {
            let mut book = B::with_config(Default::default());
            for cmd in commands {
                events.clear();
                book.apply(cmd, &mut events);
                total += events.len();
            }
        }
        total
    }
    let events = match book {
        "ref" => apply_all::<RefBook>(&journal.commands, repeats),
        "fast" => apply_all::<FastBook>(&journal.commands, repeats),
        "none" => 0,
        _ => return Err(format!("unknown book `{book}` (ref, fast or none)")),
    };
    // Printing the event count keeps the loop's result alive, so it can't be optimized away.
    println!(
        "{} commands x {repeats}: {events} events",
        journal.commands.len()
    );
    Ok(())
}

/// What the numbers were measured on (D27). Linux-only files; missing ones print "?".
fn machine() -> String {
    let read = |p: &str| fs::read_to_string(p).map(|s| s.trim().to_string());
    let cpu = read("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_else(|| "?".into());
    let pinned = read("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Cpus_allowed_list"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_else(|| "?".into());
    let or_q = |r: io::Result<String>| r.unwrap_or_else(|_| "?".into());
    let governor = or_q(read(
        "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
    ));
    let epp = or_q(read(
        "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference",
    ));
    let no_turbo = or_q(read("/sys/devices/system/cpu/intel_pstate/no_turbo"));
    format!(
        "cpu      {cpu}\nkernel   {}\ngovernor {governor} (epp {epp}, no_turbo {no_turbo})\ncpus     {pinned} (the cpus this process may run on)\n",
        or_q(read("/proc/sys/kernel/osrelease"))
    )
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
