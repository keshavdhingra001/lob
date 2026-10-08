//! `lob`: the REPL (no arguments), or the replay tools.
//!
//! ```text
//! lob                                   interactive REPL on the reference book
//! lob gen <seed> <count> <journal> [max-live]   write <count> generated commands to a journal
//! lob replay <journal> [events-file]    replay a journal; print stats and the digest
//! lob bench <journal>                   replay through both books; compare speed and digests
//! lob gen-queue <orders> <journal>      worst case: one deep queue, cancelled in random order
//! lob latency <journal> [runs] [dir]    per-command latency percentiles for both books
//! lob plot <out.svg> <title> <label=file.hgrm>...   percentile plot of latency histograms (D60)
//! lob run <ref|fast|none> <journal> [repeats]   apply only, no timing: for `perf stat` (D29)
//! lob feed <journal> [drop-percent] [seed]   publish market data; recover over a lossy link (D40–D44)
//! lob pipeline <journal> [rate] [ring|mpsc] [capacity]   three threads vs one (D45–D48)
//! lob itch <file[.gz]> [frame|decode|book|dump] [symbol]   replay a NASDAQ ITCH 5.0 day (D36–D39)
//! lob itch <file[.gz]> top              the symbols with the most add orders
//! lob itch <file[.gz]> journal <symbol> <journal>   one symbol's flow as engine commands (D54)
//! lob engine <input> <journal> <snapshot> [every] [none|batch] [batch]   live engine, recovers on start (D77–D79)
//! lob recover <journal> <snapshot> [ref|fast]   recover and print the digest (D79)
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use lob::book::apply_all;
use lob::engine::{Engine, Opened, Sync};
use lob::gen::{GenConfig, Generator};
use lob::journal::{read_journal, Journal, JournalWriter};
use lob::latency::{measure_interleaved, median_run, table, Report};
use lob::plot::{hgrm, parse_hgrm, svg};
use lob::recovery::recover_files;
use lob::replay::{replay, replay_timed};
use lob::scenario::run_line;
use lob::{BookConfig, Command, FastBook, OrderBook, RefBook};
use std::time::{Duration, Instant};

const HELP: &str = "\
commands:
  limit  <id> <buy|sell> <qty> <price> [gtc|ioc|fok|post]   prices are integer ticks
  market <id> <buy|sell> <qty>
    either may end in g=<group> stp=<cn|co|cb>               self-trade prevention: cancel
                                                             newest, oldest or both
  modify <id> <qty> <price>                                  qty = new open quantity
  cancel <id>
  book                                                       asks above bids, highest first
  config <tick_size> <max_qty>                               start over with these rules
  help | quit";

const USAGE: &str = "\
usage:
  lob                                   interactive REPL
  lob gen <seed> <count> <journal> [max-live] [stp-groups]   write generated commands to a
                                        journal file (max-live: orders the generator keeps alive,
                                        default 5000; stp-groups: new orders carry one of this many
                                        STP groups or none, default 0)
  lob replay <journal> [events-file]    replay a journal, print stats and digest
  lob bench <journal>                   replay through both books, compare speed and digests
  lob gen-queue <orders> <journal>      worst case for cancel: one deep queue, random cancels
  lob latency <journal> [runs] [dir]    per-command latency percentiles, both books
                                        (runs: default 5; pin it with `taskset -c <cpu>`); with
                                        dir, also write ref.hgrm, fast.hgrm and clock.hgrm there
  lob plot <out.svg> <title> <label=file.hgrm>...   draw up to 4 .hgrm files as a log-log
                                        percentile plot
  lob run <ref|fast|none> <journal> [repeats]   apply only, nothing timed or printed per command,
                                        for `perf stat`; `none` only decodes (the baseline)
  lob feed <journal> [drop-percent] [seed]   publish the journal's market data: messages, bytes,
                                        digest, the publisher's cost, and a consumer recovering
                                        over a link that drops (default 1%) and duplicates (5%)
  lob pipeline <journal> [rate] [ring|mpsc] [capacity]   gateway -> matching -> output threads
                                        against the same work on one thread: throughput, end-to-end
                                        latency, digests; rate 0 (default) floods, otherwise
                                        commands/s on a schedule; capacity default 1024
  lob itch <file[.gz]> [frame|decode|book|dump] [symbol]   replay a NASDAQ ITCH 5.0 file: frame
                                        only, frame + decode, or rebuild every book (default), and
                                        print messages/s; `book` also prints the symbol's depth at
                                        16:00 (default AAPL) and the D38 checks; `dump` prints the
                                        symbol's messages
  lob itch <file[.gz]> top              the 10 symbols with the most add orders
  lob itch <file[.gz]> journal <symbol> <journal>   translate one symbol's flow into engine
                                        commands (D54) and write them to a journal
  lob engine <input> <journal> <snapshot> [every] [none|batch] [batch]   run the engine live:
                                        journal each batch (default 64 commands), fsync it
                                        (batch, the default) or not (none), then apply it; snapshot
                                        every <every> commands (default 100000, 0 = never). If the
                                        journal exists it recovers first and skips the input
                                        commands the journal already holds. Input: a journal or
                                        text commands, one per line
  lob recover <journal> <snapshot> [ref|fast]   recover from the snapshot (if usable) and the
                                        journal, cut a torn tail, print stats and the digest";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        [] => repl().map_err(|e| e.to_string()),
        ["gen", seed, count, path] => gen(seed, count, path, None, None),
        ["gen", seed, count, path, max_live] => gen(seed, count, path, Some(max_live), None),
        ["gen", seed, count, path, max_live, groups] => {
            gen(seed, count, path, Some(max_live), Some(groups))
        }
        ["replay", path] => replay_file(path, None),
        ["replay", path, events] => replay_file(path, Some(events)),
        ["bench", path] => bench(path),
        ["gen-queue", n, path] => gen_queue(n, path),
        ["run", book, path] => run(book, path, "1"),
        ["run", book, path, repeats] => run(book, path, repeats),
        ["latency", path] => latency(path, "5", None),
        ["latency", path, runs] => latency(path, runs, None),
        ["latency", path, runs, dir] => latency(path, runs, Some(dir)),
        ["plot", out, title, ref series @ ..] if !series.is_empty() => plot(out, title, series),
        ["feed", path] => feed(path, "1", "1"),
        ["feed", path, drop] => feed(path, drop, "1"),
        ["feed", path, drop, seed] => feed(path, drop, seed),
        ["pipeline", path] => pipeline(path, "0", "ring", "1024"),
        ["pipeline", path, rate] => pipeline(path, rate, "ring", "1024"),
        ["pipeline", path, rate, channel] => pipeline(path, rate, channel, "1024"),
        ["pipeline", path, rate, channel, cap] => pipeline(path, rate, channel, cap),
        ["itch", path, "top"] => itch_top(path),
        ["itch", path, "journal", symbol, out] => itch_journal(path, symbol, out),
        ["itch", path] => itch_replay(path, "book", "AAPL"),
        ["itch", path, mode] => itch_replay(path, mode, "AAPL"),
        ["itch", path, mode, symbol] => itch_replay(path, mode, symbol),
        ["engine", input, journal, snap] => engine(input, journal, snap, "100000", "batch", "64"),
        ["engine", input, journal, snap, every] => {
            engine(input, journal, snap, every, "batch", "64")
        }
        ["engine", input, journal, snap, every, sync] => {
            engine(input, journal, snap, every, sync, "64")
        }
        ["engine", input, journal, snap, every, sync, batch] => {
            engine(input, journal, snap, every, sync, batch)
        }
        ["recover", journal, snap] => recover(journal, snap, "fast"),
        ["recover", journal, snap, book] => recover(journal, snap, book),
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

fn gen(
    seed: &str,
    count: &str,
    path: &str,
    max_live: Option<&str>,
    groups: Option<&str>,
) -> Result<(), String> {
    let seed = parse_arg("seed", seed)?;
    let count = parse_arg("count", count)?;
    let defaults = GenConfig::default();
    let max_live = max_live.map_or(Ok(defaults.max_live), |m| parse_arg("max-live", m))?;
    let stp_groups = groups.map_or(Ok(defaults.stp_groups), |g| parse_arg("stp-groups", g))?;
    let config = GenConfig {
        max_live,
        stp_groups,
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
    print_stats(&stats);
    let secs = elapsed.as_secs_f64();
    println!(
        "time     {:.3} s  ({:.0} commands/s, reference book, includes encoding + hashing)",
        secs,
        stats.commands as f64 / secs
    );
    Ok(())
}

/// Commands from a journal file, or from text, one per line (blank lines and `#` comments
/// skipped).
fn load_commands(path: &str) -> Result<Vec<Command>, String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.starts_with(lob::journal::MAGIC) {
        return Ok(read_journal(&bytes)
            .map_err(|e| format!("{path}: {e}"))?
            .commands);
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8"))?;
    text.lines()
        .enumerate()
        .map(|(n, l)| (n, l.trim()))
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
        .map(|(n, l)| l.parse().map_err(|e| format!("{path}:{}: {e}", n + 1)))
        .collect()
}

fn print_stats(stats: &lob::replay::ReplayStats) {
    println!(
        "commands {}  events {}  trades {}  rejects {}",
        stats.commands, stats.events, stats.trades, stats.rejects
    );
    println!("digest   {:016x}", stats.digest);
}

fn print_opened(opened: &Opened) {
    if let Some(e) = &opened.bad_snapshot {
        eprintln!("warning: {e}; recovering from the start of the journal");
    }
    if opened.fresh {
        println!("journal  new");
    } else {
        println!(
            "recovered  {} snapshot, {} records replayed, {} torn bytes cut",
            if opened.from_snapshot {
                "from the"
            } else {
                "without a"
            },
            opened.replayed,
            opened.truncated
        );
    }
}

/// The live engine (D77, D78). It stands in for a gateway with an input file: after a
/// restart it skips the commands the journal already holds, as a client resending from
/// its last acknowledged sequence number would.
fn engine(
    input: &str,
    journal: &str,
    snap: &str,
    every: &str,
    sync: &str,
    batch: &str,
) -> Result<(), String> {
    let every = parse_arg("snapshot interval", every)?;
    let batch: usize = parse_arg("batch size", batch)?;
    if batch == 0 {
        return Err("batch size must be positive".into());
    }
    let sync = match sync {
        "none" => Sync::None,
        "batch" => Sync::Batch,
        _ => return Err(format!("bad sync mode `{sync}` (none|batch)")),
    };
    let commands = load_commands(input)?;
    let started = Instant::now();
    let (mut engine, opened) = Engine::<FastBook>::open(
        journal.as_ref(),
        snap.as_ref(),
        BookConfig::default(),
        sync,
        every,
        |_| {},
    )
    .map_err(|e| format!("{journal}: {e}"))?;
    let recovery = started.elapsed();
    print_opened(&opened);
    // The journal must hold a prefix of the input, or this isn't the same session.
    let journaled = load_journal(journal)?.commands;
    if !commands.starts_with(&journaled) {
        return Err(format!(
            "{journal} holds {} commands that aren't the start of {input}: a different input?",
            journaled.len()
        ));
    }
    let todo = &commands[journaled.len()..];
    let err = |e: io::Error| format!("{journal}: {e}");
    let (mut snapshots, mut pause_max, mut pause_total) = (0u32, Duration::ZERO, Duration::ZERO);
    let start = Instant::now();
    for chunk in todo.chunks(batch) {
        engine.process(chunk, |_| {}).map_err(err)?;
        if engine.snapshot_due() {
            let t = Instant::now();
            engine.snapshot().map_err(err)?;
            let pause = t.elapsed();
            snapshots += 1;
            pause_max = pause_max.max(pause);
            pause_total += pause;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let syncs = engine.syncs;
    let book = engine.book.state().digest();
    let stats = engine.close().map_err(err)?;
    print_stats(&stats);
    println!("book     {book:016x}");
    println!(
        "time     {secs:.3} s for {} new commands ({:.0}/s), recovery {:.3} s, {syncs} fsyncs",
        todo.len(),
        todo.len() as f64 / secs,
        recovery.as_secs_f64()
    );
    if snapshots > 0 {
        println!(
            "snapshots {snapshots}, pause mean {:.2} ms, max {:.2} ms",
            pause_total.as_secs_f64() * 1e3 / snapshots as f64,
            pause_max.as_secs_f64() * 1e3
        );
    }
    Ok(())
}

fn recover(journal: &str, snap: &str, book: &str) -> Result<(), String> {
    fn run<B: OrderBook>(journal: &str, snap: &str) -> Result<(), String> {
        let start = Instant::now();
        let r = recover_files::<B>(
            journal.as_ref(),
            snap.as_ref(),
            BookConfig::default(),
            |_| {},
        )
        .map_err(|e| format!("{journal}: {e}"))?;
        let secs = start.elapsed().as_secs_f64();
        print_opened(&Opened {
            fresh: false,
            from_snapshot: r.from_snapshot,
            replayed: r.replayed,
            truncated: r.truncated,
            bad_snapshot: r.bad_snapshot,
        });
        print_stats(&r.recorder.stats());
        println!("book     {:016x}", r.book.state().digest());
        println!("time     {secs:.3} s");
        Ok(())
    }
    match book {
        "ref" => run::<RefBook>(journal, snap),
        "fast" => run::<FastBook>(journal, snap),
        _ => Err(format!("bad book `{book}` (ref|fast)")),
    }
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
/// With `dir`, the median runs' whole histograms go there as `.hgrm` files (D60).
fn latency(path: &str, runs: &str, dir: Option<&str>) -> Result<(), String> {
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
    if let Some(dir) = dir {
        let (reference, fast) = (median_run(&reference), median_run(&fast));
        fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        for (name, h) in [
            ("ref", &reference.all),
            ("fast", &fast.all),
            ("clock", &fast.clock),
        ] {
            let file = format!("{dir}/{name}.hgrm");
            fs::write(&file, hgrm(h)).map_err(|e| format!("{file}: {e}"))?;
        }
        println!("histograms written to {dir}/{{ref,fast,clock}}.hgrm");
    }
    Ok(())
}

/// Draw `.hgrm` files, each given as `label=file`, as one percentile plot (D60).
fn plot(out: &str, title: &str, series: &[&str]) -> Result<(), String> {
    if series.len() > 4 {
        return Err("at most 4 series".to_string());
    }
    let mut loaded = Vec::new();
    for arg in series {
        let (label, file) = arg
            .split_once('=')
            .ok_or_else(|| format!("`{arg}`: expected label=file.hgrm"))?;
        let text = fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        loaded.push((
            label,
            parse_hgrm(&text).map_err(|e| format!("{file}: {e}"))?,
        ));
    }
    let series: Vec<(&str, &[_])> = loaded.iter().map(|(l, p)| (*l, p.as_slice())).collect();
    fs::write(out, svg(title, &series)).map_err(|e| format!("{out}: {e}"))
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

/// The three-thread pipeline against the same work on one thread (D45–D48): 3 runs of each,
/// alternating. Prints each run's throughput and latency, and fails if any digest differs.
fn pipeline(path: &str, rate: &str, channel: &str, capacity: &str) -> Result<(), String> {
    use lob::latency::{row, HEADER};
    use lob::pipeline::{frames, run as run_pipeline, run_single, Mpsc, Report, Ring};
    use std::time::Instant;

    let rate: u64 = parse_arg("rate", rate)?;
    let capacity: usize = parse_arg("capacity", capacity)?;
    if !capacity.is_power_of_two() {
        return Err(format!("capacity {capacity} isn't a power of two"));
    }
    let run: fn(&[lob::pipeline::Frame], usize, u64) -> Report = match channel {
        "ring" => run_pipeline::<Ring>,
        "mpsc" => run_pipeline::<Mpsc>,
        _ => return Err(format!("unknown channel `{channel}` (ring or mpsc)")),
    };
    let journal = load_journal(path)?;
    let frames = frames(&journal.commands);
    let n = frames.len() as f64;
    let mega = |secs: f64| n / secs / 1e6;
    println!(
        "{} commands, {channel} capacity {capacity}, {}",
        frames.len(),
        match rate {
            0 => "flooding".to_string(),
            r => format!("paced at {r} commands/s, latency from the schedule"),
        }
    );
    let mut want = None;
    for i in 1..=3 {
        let start = Instant::now();
        let single = run_single(&frames);
        let single_secs = start.elapsed().as_secs_f64();
        let report = run(&frames, capacity, rate);
        if report.digests != single || want.is_some_and(|w| w != single) {
            return Err(format!(
                "run {i}: digests differ\n{single:?}\n{:?}",
                report.digests
            ));
        }
        want = Some(single);
        let secs = report.elapsed.as_secs_f64();
        println!(
            "run {i}: one thread {:.3} s ({:.2} M/s), pipeline {:.3} s ({:.2} M/s)",
            single_secs,
            mega(single_secs),
            secs,
            mega(secs)
        );
        println!("  {HEADER}");
        println!("  {}", row("end-to-end", &report.latency));
    }
    let d = want.expect("3 runs");
    println!(
        "events digest {:016x}  feed digest {:016x}  ({} events, {} feed messages, {} bad frames)",
        d.events_digest, d.feed_digest, d.events, d.feed_msgs, d.bad_frames
    );
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

/// The 10 symbols with the most add orders (`A` and `F`), to pick D54's workloads.
fn itch_top(path: &str) -> Result<(), String> {
    use lob::itch::{self, Body};

    let mut reader = itch::open(path.as_ref()).map_err(|e| format!("{path}: {e}"))?;
    let (mut adds, mut names) = (vec![0u64; 1 << 16], vec![None; 1 << 16]);
    while let Some(m) = reader.next_message().map_err(|e| format!("{path}: {e}"))? {
        match m.body {
            Body::StockDirectory { stock } => names[m.header.locate as usize] = Some(stock),
            Body::AddOrder { .. } => adds[m.header.locate as usize] += 1,
            _ => {}
        }
    }
    let mut top: Vec<(u64, usize)> = adds.iter().enumerate().map(|(l, &n)| (n, l)).collect();
    top.sort_by(|a, b| b.cmp(a));
    for &(n, l) in top.iter().take(10).filter(|(n, _)| *n > 0) {
        let name = names[l].map_or("?".to_string(), |s| s.to_string());
        println!("{name:<8} {n:>10} adds  (locate {l})");
    }
    Ok(())
}

/// Translate one symbol's flow into engine commands (D54) and write them to a journal.
fn itch_journal(path: &str, symbol: &str, out: &str) -> Result<(), String> {
    use lob::itch::{self, Stock};
    use lob::itch_flow::Translator;

    if symbol.is_empty() || symbol.len() > 8 {
        return Err(format!("bad symbol `{symbol}`"));
    }
    let mut reader = itch::open(path.as_ref()).map_err(|e| format!("{path}: {e}"))?;
    let mut translator = Translator::new(Stock::new(symbol));
    let mut commands = Vec::new();
    while let Some(m) = reader.next_message().map_err(|e| format!("{path}: {e}"))? {
        translator.on_message(&m, &mut commands);
    }
    write_journal(out, commands.iter().copied())?;
    let s = translator.stats();
    if s.messages == 0 {
        return Err(format!("no messages for `{symbol}`"));
    }
    let resting: usize = [lob::Side::Buy, lob::Side::Sell]
        .iter()
        .flat_map(|&side| translator.book().depth(side, usize::MAX))
        .map(|l| l.orders)
        .sum();
    println!(
        "{symbol}: {} messages -> {} commands in {out}",
        s.messages, s.commands
    );
    println!(
        "adds {} (sub-penny, skipped: {}; traded on arrival: {}), executions {}, cross executions {}",
        s.adds, s.sub_penny, s.adds_traded, s.executions, s.cross_executions
    );
    let shares = (s.named_shares + s.other_shares + s.unfilled_shares).max(1) as f64;
    println!(
        "IOC shares: {} filled the named order ({:.2}%), {} another order, {} unfilled",
        s.named_shares,
        s.named_shares as f64 / shares * 100.0,
        s.other_shares,
        s.unfilled_shares
    );
    println!(
        "resync cancels {}, skipped (gone from our book) {}, untracked {}, rejects {}; {resting} orders resting at the end",
        s.resyncs, s.gone, s.untracked, s.rejects
    );
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
