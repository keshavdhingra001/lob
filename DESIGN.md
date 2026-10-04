# lob design

Living document. Every non-obvious decision gets a short entry: **what**, **alternatives**, **why**.

## Architecture (target, after M9)

```
clients ──> gateway thread ──SPSC ring──> matching thread ──SPSC ring──> market data / journal
                 │                         (one per symbol)                    │
                 └── decode + validate     Command in, Events out          L2 updates, trades
                                           pure, deterministic             command journal (replay)
```

Today (M1): the reference book (D8) behind the `OrderBook` trait, matching by D9, checked by
scenario scripts and an invariant checker (D10).

## Decisions

### D1: Language: Rust
- **Alternatives:** C++20, the default in HFT.
- **Why:** same toolchain and workflow as lsmkv (cargo, clippy, criterion, proptest, cargo-fuzz),
  and no undefined behaviour to chase while learning. The parts HFT interviews probe (cache
  layout, allocation-free hot paths, lock-free queues, tail latency) all exist in Rust and get
  measured here. Trade-off: C++-only shops will ask C++ questions anyway.

### D2: Integer prices and quantities
- **What:** `Price(i64)` in ticks, `Qty(u64)` in whole units. With a $0.01 tick, $100.25 is `Price(10025)`.
- **Alternatives:** `f64` (wrong), a decimal type (slow, unnecessary).
- **Why:** prices are used as exact keys. `0.1 + 0.2 != 0.3` in `f64`, so two orders at the "same"
  price could land on different levels, or a buy at 0.3 could fail to cross a sell at 0.1 + 0.2.
  Integer compare is also one instruction. Real feeds do the same (ITCH sends prices as integers
  with 4 implied decimals). Prices are signed because spreads and some futures go negative.
- **Tick size and conversion to dollars live outside the engine** (per-instrument config, M2).

### D3: Client-assigned order ids
- **What:** every `limit`/`market` command carries its own `OrderId`, and `cancel` refers to it.
- **Alternatives:** the engine assigns ids and returns them in `Accepted`.
- **Why:** the command stream alone fully determines the run, so a journal of commands replays
  exactly (M3) without needing the engine's output. Exchanges work the same way (FIX `ClOrdID`).
  Uniqueness rules are an M1 decision.

### D4: Deterministic state machine
- **What:** `apply(&Command) -> Events` with no clock, no randomness, no I/O and no dependence on
  `HashMap` iteration order. Timestamps, if needed, arrive inside commands.
- **Why:** this is what makes the project concrete: replay a day of order flow and get
  byte-identical output (M3), and run two book implementations side by side and diff their
  events (M4). It's how real exchanges do recovery and failover (LMAX, Nasdaq's sequenced
  matching engines): replicas apply the same sequenced input and so reach the same state.

### D5: Caller-owned output buffer
- **What:** `apply(&mut self, cmd: &Command, out: &mut Vec<Event>)` appends to `out` instead of returning a `Vec`.
- **Why:** returning a new `Vec` allocates on every command. With a reused buffer the steady
  state allocates nothing, which M6 will prove with a counting allocator.

### D6: Text command format (for the REPL and tests)
- **What:** one command per line: `limit <id> <side> <qty> <price>`, `market <id> <side> <qty>`,
  `cancel <id>`. `Display` prints exactly what `FromStr` parses (round-trip tested). Events print
  one line each, for scenario tests to compare against.
- **Why:** readable scenario files and REPL sessions. The binary journal format for replay is a
  separate decision in M3, where size and decode speed matter.

### D7: Single-threaded matching per symbol (planned, revisit in M9)
- **What:** one thread owns a book. Concurrency lives at the edges (gateway decode, market data
  fan-out), connected by single-producer single-consumer ring buffers.
- **Alternatives:** locks around a shared book; concurrent data structures inside the book.
- **Why:** matching is inherently sequential (price-time priority is a total order), so locking
  only adds contention and makes results depend on thread timing, which breaks D4. Scaling
  comes from sharding symbols across threads. This is the LMAX Disruptor design.

### D8: The reference book (M1)
- **What:** per side, a `BTreeMap<Price, VecDeque<Order>>`: sorted price levels, each a FIFO queue.
  A `HashMap<OrderId, (Side, Price)>` finds a resting order's level for cancel, which then scans
  that level's queue.
- **Costs:** a new order that rests is O(log P) for P price levels; each fill is O(1) plus
  O(log P) when a level empties; cancel is O(log P + orders at that level).
- **Why:** it's the oracle. Every later book is checked against it, so it has to be obviously
  correct, not fast. M4's fast book must produce identical events.
- **Rejected alternatives:**
  - Storing each order's index in its queue: an index shifts whenever an order ahead of it leaves.
  - A sorted `Vec` of levels: O(P) inserts. A tick-indexed array is what M6 measures.

### D9: Matching rules (M1)
- **Price-time priority:** best price first. Within a price, the oldest order goes first, and a
  partial fill keeps its place in the queue.
- **Trade price:** always the resting (maker) order's price. A buy limit at 10250 that hits an ask
  at 10100 pays 10100. That's price improvement for the taker.
- **One `trade` per maker order filled**, rather than one per price level, so every resting
  order's fills can be traced (this matters for M8's market data and for differential tests).
- **Market orders** fill what the book has, then `cancelled <id> <rest>`. They never rest.
  Rejecting a market order that can't fully fill needs a pre-scan, and that's what FOK (M2) is for.
- **Event order per command:** `accepted`, then trades in fill order, then `cancelled` if a
  remainder was dropped. A limit order that rests produces no extra event; resting is implied.
- **Rejects change nothing:** zero quantity, a reused id, or a cancel of an id that isn't resting.
  A rejected order doesn't use up its id.
- **Id uniqueness:** any id accepted this session can never be used again, even after the order
  is done. Otherwise a late cancel meant for the old order could hit the new one. The `used` set
  grows with the session. M6 will measure it (an alternative is to require increasing ids, which
  costs O(1) memory).
- **Not yet:** modify, IOC/FOK/post-only, tick size (M2); self-trade prevention (Tier 3).

### D10: Scenario tests (M1)
- **What:** `tests/scenarios/*.txt` are scripts in the D6 format. Each command is followed by its
  expected events as `> ` lines, and `book` prints the whole book as a ladder.
  The runner drops the `>` lines, re-runs the script and regenerates them, so a scenario passes
  when it equals its own transcript. On failure it shows the first line that differs.
- **The invariant checker runs after every command:** never crossed, no empty levels, no zero
  quantities, and the index agrees with the queues.
- **Same runner for every book:** M4 runs the same files against the fast book. The REPL uses it
  too, so anything typed there can be pasted into a scenario.
- **Mutation-checked:** 8 planted bugs, all caught:
  - the worst ask first, or LIFO within a level
  - trading at the taker's limit, or the crosses check inverted
  - the market remainder not cancelled
  - filled orders left in the index, or duplicate ids accepted
  - empty levels not removed (this one made matching loop forever, so it was caught by the timeout)
