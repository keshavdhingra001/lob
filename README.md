# lob

A limit order book and matching engine written from scratch in Rust: price-time priority
matching, deterministic replay, and measured tail latency.

> Work in progress. See [CHECKPOINT.md](CHECKPOINT.md) for status and the roadmap, and
> [DESIGN.md](DESIGN.md) for design decisions.

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

## What's built so far

- **Zero-allocation hot path**: once warmed up, the fast book applies millions of commands without a
  single heap allocation, proven by a counting allocator. Order ids must increase per session (as on
  Nasdaq OUCH), price levels live in a tick-indexed ladder with a two-level bitmap, and the id
  index uses a cheap MurmurHash3 finalizer.
- **Latency measurement**: per-command p50 / p99 / p99.9 / max for each kind of command
  (`lob latency`, HdrHistogram), the clock's own cost reported alongside, runs that alternate
  between the books on a pinned core, and criterion microbenchmarks at fixed book depths.
- **Fast book**: a slab of orders with an intrusive doubly linked list per price level, O(1)
  cancel by id, and a cached best level. It produces identical events to the reference book over
  15 million differential-tested commands.
- **Reference book**: price-time priority matching for limit, market and cancel. Every trade
  happens at the resting order's price. It's built to be obviously correct, and it's the oracle
  the fast book (M4) is tested against.
- **Scenario tests**: scripts of commands and their expected events
  ([tests/scenarios](tests/scenarios)), checked against every book. An invariant checker runs
  after every command, and the tests are mutation-checked.
- **Order lifecycle**: modify with exchange-style priority rules (a lower size keeps your place,
  anything else moves you to the back), plus IOC, fill-or-kill and post-only orders, and tick size
  and fat-finger quantity limits.
- **Conservation ledger**: an outside check that rebuilds every order's open quantity from the
  event stream alone and matches it against the book after each command, over thousands of
  random sessions.
- **Deterministic replay**: a checksummed binary command journal (torn tails tolerated,
  mid-file damage refused) and a sequence-numbered binary event stream with a 64-bit digest.
  Replays are byte-identical.
- **Synthetic order flow**: a seeded generator with a random-walk mid, queues building at the
  touch, and frequent cancels and modifies.
- **Command and event model** with a text format whose parser and printer round-trip.

## Results so far

Throughput on one core (`lob bench`, release, best of 5, matching only):

| Workload | Reference book | Fast book | Speedup |
|---|---|---|---|
| Generated order flow, 2M commands | 13.6 M/s | 16.1 M/s | 1.19x |
| One queue of 50,000 orders, cancelled in random order | 0.30 M/s | 15.2 M/s | 51x |

On realistic flow both books spend most of their time on the same costs (hashing, the price tree,
emitting events). The fast book's structure guarantees cancels cost the same however deep a
queue gets. Details are in [DESIGN.md](DESIGN.md) (D22).

Latency per cancel (`scripts/latency.sh`, release, one pinned laptop core, ns, includes about 14 ns of clock cost):

| Workload | Reference p50 / p99 | Fast p50 / p99 |
|---|---|---|
| Generated order flow, 2M commands | 106 / 179 | 49 / 106 |
| One queue of 10,000 orders, cancelled in random order | 1,086 / 3,557 | 44 / 61 |

Limit, market and reject latencies are the same in both books.

After M6 (zero allocations, increasing ids, a price ladder), on the same 2M generated commands, fast book:

| | M5 | M6 |
|---|---|---|
| p50 / p99, all commands | 57 / 297 ns | 42 / 186 ns |
| Resting a limit order, p50 / p99 | 98 / 264 ns | 50 / 95 ns |
| Worst case | 10 ms | 0.13 ms |
| CPU cycles per command | 281 | 124 |

The 10 ms worst case was a set of every order id ever used, rehashing as it grew. Details are in [DESIGN.md](DESIGN.md) (D30–D34, M6 results).

M7 replays a real NASDAQ TotalView-ITCH 5.0 day (30 July 2019, 282M messages) and rebuilds the book of every symbol:

| | |
|---|---|
| Errors (unknown order, overfill, wrong symbol) | 0 in 277M book messages |
| Orders left at the end of the day | 0 of 125.5M added |
| `E` executions at the best price | 7,582,422 of 7,582,422 |
| Crossed books outside auction unwinds | 0 |
| Throughput: frame / decode / full rebuild | 43 / 29 / 3.8 M messages/s (uncompressed); rebuild 3.1 M/s from `.gz` |

`lob itch <file> [frame|decode|book|dump] [symbol]`. The sample files are at emi.nasdaq.com/ITCH and aren't in the repo.
Details are in [DESIGN.md](DESIGN.md) (D35–D39, M7 results).

## Planned

- A lock-free pipeline between gateway, matching and market data threads.
