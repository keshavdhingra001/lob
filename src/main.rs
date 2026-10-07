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
//! lob feed <journal> [drop-percent] [seed]   publish market data; recover over a lossy link (D40–D44)
//! lob itch <file[.gz]> [frame|decode|book|dump] [symbol]   replay a NASDAQ ITCH 5.0 day (D36–D39)
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use lob::book::apply_all;
use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, Journal, JournalWriter};
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
                                        for `perf stat`; `none` only decodes (the baseline)
  lob feed <journal> [drop-percent] [seed]   publish the journal's market data: messages, bytes,
                                        digest, the publisher's cost, and a consumer recovering
                                        over a link that drops (default 1%) and duplicates (5%)
  lob itch <file[.gz]> [frame|decode|book|dump] [symbol]   replay a NASDAQ ITCH 5.0 file: frame
                                        only, frame + decode, or rebuild every book (default), and
                                        print messages/s; `book` also prints the symbol's depth at
                                        16:00 (default AAPL) and the D38 checks; `dump` prints the
                                        symbol's messages";

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
        ["feed", path] => feed(path, "1", "1"),
        ["feed", path, drop] => feed(path, drop, "1"),
        ["feed", path, drop, seed] => feed(path, drop, seed),
        ["itch", path] => itch_replay(path, "book", "AAPL"),
        ["itch", path, mode] => itch_replay(path, mode, "AAPL"),
        ["itch", path, mode, symbol] => itch_replay(path, mode, symbol),
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

/// A numeric argument, or "bad <what> `<arg>`".
fn parse_arg<T: std::str::FromStr>(what: &str, arg: &str) -> Result<T, String> {
    arg.parse().map_err(|_| format!("bad {what} `{arg}`"))
}

fn load_journal(path: &str) -> Result<Journal, String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    read_journal(&bytes).map_err(|e| format!("{path}: {e}"))
}

fn write_journal(path: &str, commands: impl IntoIterator<Item = Command>) -> Result<(), String> {
    let err = |e: io::Error| format!("{path}: {e}");
    let file = File::create(path).map_err(err)?;
    let mut journal = JournalWriter::new(BufWriter::new(file)).map_err(err)?;
    for cmd in commands {
        journal.append(&cmd).map_err(err)?;
    }
    journal.finish().map(drop).map_err(err)
}

fn gen(seed: &str, count: &str, path: &str, max_live: Option<&str>) -> Result<(), String> {
    let seed = parse_arg("seed", seed)?;
    let count = parse_arg("count", count)?;
    let max_live = match max_live {
        Some(m) => parse_arg("max-live", m)?,
        None => GenConfig::default().max_live,
    };
    let config = GenConfig {
        max_live,
        ..GenConfig::with_seed(seed)
    };
    write_journal(path, Generator::new(config).take(count))?;
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    println!("wrote {count} commands ({size} bytes) to {path}");
    Ok(())
}

fn replay_file(path: &str, events_path: Option<&str>) -> Result<(), String> {
    let journal = load_journal(path)?;
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
    let n = parse_arg("order count", n)?;
    write_journal(path, lob::gen::deep_queue(n, 1))?;
    println!("wrote {n} queued orders + {n} cancels to {path}");
    Ok(())
}

/// Best of 5 runs per book, each on a fresh book. Whole-session throughput only:
/// per-command latency percentiles are M5.
fn bench(path: &str) -> Result<(), String> {
    let journal = load_journal(path)?;
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
            let start = std::time::Instant::now();
            apply_all(&mut book, commands);
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
    let journal = load_journal(path)?;
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
    let repeats: usize = parse_arg("repeat count", repeats)?;
    let journal = load_journal(path)?;
    fn repeat<B: OrderBook>(commands: &[Command], repeats: usize) -> usize {
        (0..repeats)
            .map(|_| apply_all(&mut B::with_config(Default::default()), commands))
            .sum()
    }
    let events = match book {
        "ref" => repeat::<RefBook>(&journal.commands, repeats),
        "fast" => repeat::<FastBook>(&journal.commands, repeats),
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

/// Publish a journal's market data (D40–D44): message and byte counts and the feed's digest,
/// the publisher's cost (best of 5, alternating with apply-only runs), and a consumer
/// recovering over a lossy link, checked against the engine's book at the end.
fn feed(path: &str, drop_pct: &str, seed: &str) -> Result<(), String> {
    use lob::consumer::{Consumer, Link};
    use lob::feed::{self, Msg, Publisher};
    use lob::replay::Fnv64;
    use std::time::Instant;

    let drop_pct: u64 = parse_arg("drop percent", drop_pct)?;
    if drop_pct > 100 {
        return Err(format!("drop percent {drop_pct} is over 100"));
    }
    let seed = parse_arg("seed", seed)?;
    let journal = load_journal(path)?;
    let commands = &journal.commands;
    let n = commands.len() as f64;

    // Apply + publish + encode: everything a feed handler does before the network.
    let publish = |bytes: &mut Vec<u8>| -> Result<(u64, u64, u64), String> {
        let mut book = FastBook::new();
        let mut publisher = Publisher::new();
        let (mut events, mut msgs) = (Vec::with_capacity(64), Vec::with_capacity(64));
        let (mut levels, mut trades, mut hash) = (0, 0, Fnv64::default());
        for cmd in commands {
            events.clear();
            msgs.clear();
            book.apply(cmd, &mut events);
            publisher.on_command(cmd, &events, &mut msgs)?;
            bytes.clear();
            for msg in &msgs {
                match msg {
                    Msg::Level { .. } => levels += 1,
                    Msg::Trade { .. } => trades += 1,
                }
                feed::encode(msg, bytes);
            }
            hash.update(bytes);
        }
        Ok((levels, trades, hash.finish()))
    };
    let (mut apply_best, mut publish_best) = (f64::MAX, f64::MAX);
    let mut bytes = Vec::with_capacity(4096);
    let mut counts = (0, 0, 0);
    for _ in 0..5 {
        let start = Instant::now();
        apply_all(&mut FastBook::new(), commands);
        apply_best = apply_best.min(start.elapsed().as_secs_f64());
        let start = Instant::now();
        counts = publish(&mut bytes)?;
        publish_best = publish_best.min(start.elapsed().as_secs_f64());
    }
    let (levels, trades, digest) = counts;
    let total_bytes = levels * feed::LEVEL_LEN as u64 + trades * feed::TRADE_LEN as u64;
    println!(
        "{} commands: {levels} level updates, {trades} trades, {total_bytes} bytes ({:.1} per command)",
        commands.len(),
        total_bytes as f64 / n
    );
    println!("digest   {digest:016x}");
    println!(
        "fast book, best of 5: apply {:.1} ns/command, apply + publish + encode {:.1} ns/command (+{:.1})",
        apply_best / n * 1e9,
        publish_best / n * 1e9,
        (publish_best - apply_best) / n * 1e9
    );

    let mut book = FastBook::new();
    let mut publisher = Publisher::new();
    let mut consumer = Consumer::new();
    let mut link = Link::new(seed, drop_pct, 5);
    let (mut events, mut msgs) = (Vec::new(), Vec::new());
    for cmd in commands {
        events.clear();
        msgs.clear();
        book.apply(cmd, &mut events);
        publisher.on_command(cmd, &events, &mut msgs)?;
        link.deliver(&publisher, &msgs, &mut consumer);
    }
    link.settle(&publisher, &mut consumer);
    let s = consumer.stats();
    println!(
        "link (drop {drop_pct}%, duplicate 5%, seed {seed}): {} gaps, {} snapshots, {} duplicates ignored, {} bytes delivered",
        s.gaps, s.snapshots, s.duplicates, link.bytes
    );
    let same = [lob::Side::Buy, lob::Side::Sell]
        .iter()
        .all(|&side| consumer.depth(side, usize::MAX) == book.depth(side, usize::MAX));
    if !same {
        return Err("the consumer's final book differs from the engine's".to_string());
    }
    println!("consumer's final book matches the engine's");
    Ok(())
}

/// Replay an ITCH file (D39). Timing lives here, outside the book, as with `lob bench`.
fn itch_replay(path: &str, mode: &str, symbol: &str) -> Result<(), String> {
    use lob::itch::{self, Stock};
    use lob::itch_book::{ItchBook, Phase};
    use lob::Side;

    if !["frame", "decode", "book", "dump"].contains(&mode) {
        return Err(format!(
            "unknown mode `{mode}` (frame, decode, book or dump)"
        ));
    }
    let want = Stock::new(symbol);
    let mut reader = itch::open(path.as_ref()).map_err(|e| format!("{path}: {e}"))?;
    let err = |e: itch::ItchError| format!("{path}: {e}");
    let mut by_type = [0u64; 256];
    // Folded over decoded fields so the decode loop's work can't be optimized away.
    let mut fold = 0u64;
    let mut book = ItchBook::with_capacity(if mode == "book" { 1 << 22 } else { 0 });
    let mut snapshot = None;
    let start = std::time::Instant::now();
    match mode {
        "frame" => {
            while let Some(b) = reader.next_frame().map_err(err)? {
                by_type[b[0] as usize] += 1;
            }
        }
        "decode" => {
            while let Some(m) = reader.next_message().map_err(err)? {
                fold ^= m.header.timestamp;
            }
        }
        "dump" => {
            // Every message for one symbol, plus system events: for looking into a check.
            let mut locate = None;
            while let Some(m) = reader.next_message().map_err(err)? {
                if let itch::Body::StockDirectory { stock } = m.body {
                    if stock == want {
                        locate = Some(m.header.locate);
                    }
                }
                let ours = locate == Some(m.header.locate);
                if ours || matches!(m.body, itch::Body::SystemEvent { .. }) {
                    println!("{} {:?}", clock(m.header.timestamp), m.body);
                }
            }
        }
        _ => {
            while let Some(m) = reader.next_message().map_err(err)? {
                let before = book.phase();
                book.apply(&m)
                    .map_err(|e| format!("message {}: {e} ({m:?})", reader.count() - 1))?;
                if before == Phase::Market && book.phase() == Phase::Post {
                    snapshot = book
                        .symbols()
                        .find(|(_, s)| s.stock == want)
                        .map(|(_, s)| (s.depth(Side::Buy, 5), s.depth(Side::Sell, 5), s.levels()));
                }
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let n = reader.count();
    let mb = reader.offset() as f64 / 1e6;
    println!(
        "{mode}: {n} messages, {mb:.0} MB uncompressed, {secs:.2} s: {:.1} M messages/s, {:.0} MB/s",
        n as f64 / secs / 1e6,
        mb / secs
    );
    if mode == "frame" {
        let mut counts: Vec<(u64, char)> = (0..256)
            .filter(|&t| by_type[t] > 0)
            .map(|t| (by_type[t], t as u8 as char))
            .collect();
        counts.sort_by(|a, b| b.cmp(a));
        let line: Vec<String> = counts.iter().map(|(c, t)| format!("{t} {c}")).collect();
        println!("by type: {}", line.join(", "));
    }
    if mode == "decode" {
        println!("(fold {fold:x})");
    }
    if mode == "book" {
        let s = book.stats();
        println!(
            "book messages {}, live orders at the end {}, peak {}",
            s.book_messages, s.live_orders, s.peak_live_orders
        );
        println!(
            "crossed (pre, market, post): {:?}, after the open: {}",
            s.crossed, s.crossed_after_open
        );
        println!(
            "locked  (pre, market, post): {:?}, after the open: {}",
            s.locked, s.locked_after_open
        );
        println!(
            "crossed or locked while a cross was unwinding: {}",
            s.crossed_while_uncrossing
        );
        println!(
            "E at best (pre, market, post): {:?}, not at best: {:?}; C executions: {}",
            s.executed_at_best, s.executed_not_at_best, s.executed_with_price
        );
        for c in &s.examples {
            println!(
                "  example: {} at {} bid {} ask {}",
                c.stock,
                clock(c.timestamp),
                c.bid,
                c.ask
            );
        }
        match snapshot {
            None => {
                println!("{symbol}: no snapshot (not in the directory, or no end of market hours)")
            }
            Some((bids, asks, levels)) => {
                println!(
                    "{symbol} at the end of market hours ({levels} levels), price  shares  orders:"
                );
                for (p, l) in asks.iter().rev() {
                    println!(
                        "  ask {:>10.4} {:>8} {:>4}",
                        *p as f64 / 1e4,
                        l.shares,
                        l.orders
                    );
                }
                for (p, l) in &bids {
                    println!(
                        "  bid {:>10.4} {:>8} {:>4}",
                        *p as f64 / 1e4,
                        l.shares,
                        l.orders
                    );
                }
            }
        }
    }
    Ok(())
}

/// Nanoseconds since midnight as HH:MM:SS.nnnnnnnnn.
fn clock(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        s / 3600,
        s / 60 % 60,
        s % 60,
        ns % 1_000_000_000
    )
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
