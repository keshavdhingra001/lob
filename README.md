# lob

A limit order book and matching engine in Rust, written from scratch: price-time priority matching, deterministic replay,
a lock-free three-thread pipeline, an L2 market data feed, and a real NASDAQ trading day as test input. Two book implementations,
a simple one and a fast one, must produce the same events for every command, and every performance claim cites a measurement.

| On one laptop core (i7-1165G7) | Fast book | Reference book |
|---|---|---|
| Throughput on generated and real order flow (4 journals) | **28–31 M commands/s** | 12–20 M/s |
| Latency per command on real AAPL flow, p50 / p99 | **48 / 91 ns** | 86 / 230 ns |
| Cancel in a 10,000-order queue, p50 | **41 ns** | 1,068 ns |
| Heap allocations per command, once warm | **0** | |

- **Matching agrees with NASDAQ:** replaying one trading day of AAPL and SPY, 98.7% and 100% of executed shares fill the same order NASDAQ's
  engine filled. The rest is NASDAQ's entry-time priority for orders it displays late, which an engine that only sees arrival order can't reproduce.
- **A whole NASDAQ day** (282M messages, every symbol's book rebuilt): 0 errors, 0 orders left at the end of the day, every execution message at the best price.

Sources: [BENCHMARKS.md](BENCHMARKS.md) (method, machine, every table tied to raw output in [`bench/results/`](bench/results/)),
and [DESIGN.md](DESIGN.md) for the allocation proof (D32) and the ITCH day (M7 results).

![AAPL latency percentiles, reference vs fast book](bench/results/2026-10-08-0434/latency-aapl.svg)

Up to p99.9 the fast book is about 2x lower. Past p99.99 the books meet, and so does the grey line, an empty timed window:
that part of the tail is the machine (interrupts, preemption), not the code. [More plots](BENCHMARKS.md#latency-percentile-plots-m12-d60).

## Architecture

```
 journal file ──> gateway thread ──SPSC ring──> matching thread ──SPSC ring──> output thread
                  decode, stamp                 fast book: apply()             encode + digest
                                                Command in, Events out         L2 feed publisher
                                                no clock, no I/O               end-to-end latency
```

- **The engine is one call:** `apply(&Command, &mut Vec<Event>)`. It never reads a clock, never uses randomness and never iterates a hash map,
  so the same commands always give byte-identical events (checked by a pinned 64-bit digest).
- **The fast book:** a slab of orders with an intrusive doubly linked list per price level (O(1) cancel), a tick-indexed price ladder
  with a two-level bitmap to find the best price, a cheap hasher for the id index, and no allocation once warm.
- **The pipeline:** a hand-written bounded SPSC ring (cache-line-padded indices, `Acquire`/`Release`). Its output equals one thread's byte for byte.
  At full load three threads are *slower* than one: the output stage dominates, and the hand-offs between cores cost more than they save.
- **Market data:** level updates coalesced per command, with sequence numbers, heartbeats and snapshots. A consumer detects gaps and recovers.

## How correctness is checked

- **Two books, one answer:** the fast book is tested against the reference book event for event, over 15M commands (D22).
- **Oracles that don't trust the book:** an invariant checker after every command, and a ledger that rebuilds every order's open quantity
  from the event stream alone (D10, D13).
- **Scenario scripts** of commands and expected events ([tests/scenarios](tests/scenarios)), run against both books.
- **Property tests** (proptest): codecs round-trip and accept only canonical bytes, decoders never panic, a damaged journal never yields a changed command,
  the books agree on any session, a consumer survives any loss pattern (D49–D52).
- **Real data:** a NASDAQ ITCH 5.0 day rebuilt with zero errors (D38), and one symbol's flow translated into engine commands so our matching
  is compared with NASDAQ's (D54).
- **Mutation checks:** every milestone plants bugs on purpose and confirms a test catches each one.

## Try it

```bash
cargo test
cargo run
```

```
> limit 1 sell 100 10100
accepted 1
> limit 2 buy 30 10100
accepted 2
trade 2 1 buy 30 10100
> limit 3 buy 50 10050
accepted 3
> book
ask 10100 70 (1)
bid 10050 50 (1)
```

Prices are integer ticks (`10025` is $100.25 with a one-cent tick). A trade line reads
`trade <taker> <maker> <taker side> <qty> <price>`.

Record synthetic order flow and replay it deterministically:

```bash
cargo run --release -- gen 1 20000 flow.jrnl
cargo run --release -- replay flow.jrnl events.bin
```

```
commands 20000  events 28834  trades 8316  rejects 5519
digest   f0cd0c4be21b0c27
```

The digest is the same on every run and every machine. It's pinned in the tests.

Measure it yourself (about 15 minutes, needs a quiet machine; the ITCH parts need NASDAQ's sample file, which isn't in the repo):

```bash
scripts/report.sh
```

## Code layout

| | |
|---|---|
| [`command.rs`](src/command.rs), [`types.rs`](src/types.rs), [`book.rs`](src/book.rs) | Commands, events, integer prices, the `OrderBook` trait |
| [`reference.rs`](src/reference.rs) | The reference book: `BTreeMap` of `VecDeque`s, obviously correct |
| [`fast.rs`](src/fast.rs), [`ladder.rs`](src/ladder.rs), [`hash.rs`](src/hash.rs) | The fast book, its price ladder, its id hasher |
| [`journal.rs`](src/journal.rs), [`replay.rs`](src/replay.rs) | Binary command journal, sequenced events, digest |
| [`scenario.rs`](src/scenario.rs), [`ledger.rs`](src/ledger.rs), [`gen.rs`](src/gen.rs), [`rng.rs`](src/rng.rs) | Test oracles and seeded order flow |
| [`feed.rs`](src/feed.rs), [`consumer.rs`](src/consumer.rs) | L2 market data out, and a consumer that recovers from gaps |
| [`ring.rs`](src/ring.rs), [`pipeline.rs`](src/pipeline.rs) | SPSC ring, three-thread pipeline |
| [`itch.rs`](src/itch.rs), [`itch_book.rs`](src/itch_book.rs), [`itch_flow.rs`](src/itch_flow.rs) | NASDAQ ITCH 5.0 parser, book rebuild, translation into engine commands |
| [`latency.rs`](src/latency.rs), [`plot.rs`](src/plot.rs), [`benches/`](benches/), [`scripts/`](scripts/) | Measurement harness, percentile plots, criterion, the report script |
| [`main.rs`](src/main.rs) | The `lob` CLI: REPL, `gen`, `replay`, `bench`, `latency`, `plot`, `feed`, `pipeline`, `itch` |

## Not built

No network gateway (input is a journal file, so every latency is in-process), one symbol, no auctions, no hidden or iceberg orders,
no self-trade prevention, no risk checks beyond a fat-finger quantity limit, no crash recovery beyond replaying the journal.
[DESIGN.md](DESIGN.md#not-built-d61) says where each would go.

## Build history

- [x] **M0** Scaffold: command/event model, text format, `OrderBook` trait, REPL
- [x] **M1** Reference book: price-time priority, limit / market / cancel, scenario tests, invariant checker
- [x] **M2** Order lifecycle: modify, IOC / FOK / post-only, tick and max-quantity rules, a conservation ledger
- [x] **M3** Deterministic replay: binary journal, sequenced events, golden digest, order-flow generator
- [x] **M4** Fast book: slab, intrusive lists, O(1) cancel; identical events to M1 over 15M commands
- [x] **M5** Latency measurement: per-command histograms, criterion, `perf`
- [x] **M6** Zero allocations per command, tick-indexed price ladder, cache-line layout
- [x] **M7** Real market data: NASDAQ ITCH 5.0 parser, every symbol's book rebuilt from a sample day
- [x] **M8** Market data out: L2 snapshots, incremental updates with sequence numbers, gap recovery
- [x] **M9** Engine pipeline: gateway -> lock-free SPSC ring -> matching -> output ring
- [x] **M10** Property tests (proptest): codecs, journal damage, engine and feed
- [x] **M11** Benchmark report: one script, real ITCH flow through both books, BENCHMARKS.md
- [x] **M12** Final write-up: DESIGN overview and index, percentile plots, this README

Each milestone's decisions are in [DESIGN.md](DESIGN.md), numbered D1–D63.
