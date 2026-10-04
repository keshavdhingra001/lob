# lob design

Living document. Every non-obvious decision gets a short entry: **what**, **alternatives**, **why**.

## Architecture (target, after M9)

```
clients ──> gateway thread ──SPSC ring──> matching thread ──SPSC ring──> market data / journal
                 │                         (one per symbol)                    │
                 └── decode + validate     Command in, Events out          L2 updates, trades
                                           pure, deterministic             command journal (replay)
```

Today (M5): two books behind the `OrderBook` trait: the reference book (D8), matching by D9 with modify
(D11), IOC/FOK/post-only (D12) and instrument rules (D14). Checked by scenario scripts, an
invariant checker (D10) and an event-only conservation ledger over random sessions (D13).
Input is recorded in a binary journal (D15, D16); replay produces a sequenced event stream and a
digest (D17). A seeded generator (D18) supplies realistic order flow. The fast book (D19–D21)
produces identical events, proven by differential testing over 15M commands (D22). A latency
harness outside the engine (D23–D27) and criterion benches (D28) measure both books per command kind.

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

### D11: Modify (M2)
- **What:** `modify <id> <qty> <price>` sets a resting order's new *open* quantity and price.
  Emits `modified <id> <qty> <price>`, then any trades the new price causes.
- **Priority rule:** same price and the same or lower quantity: changed in place, and it keeps its
  queue position. Anything else (a new price, or more quantity) takes the order out and re-enters
  it at the back of its new level. It may trade on the way, like a new order.
  - **Why:** keeping priority while *adding* quantity would let someone queue a tiny order early
    and grow it later, jumping everyone who arrived in between. Every major exchange (CME, Nasdaq,
    LSE) resets priority on a size increase or a price change.
- **Open quantity, not total quantity:** FIX cancel/replace sends the *total* order quantity, and
  the exchange subtracts what's already filled. That races with fills in flight (the client
  doesn't know about a fill yet). Open quantity is simpler and explicit; a gateway (Tier 3) would
  translate.
- **Rejects:** unknown or finished order, zero quantity (cancel is the explicit way out), the
  validation rules (D14), and a post-only order whose new price would cross (D12).
  A rejected modify changes nothing.

### D12: Time in force (M2)
- **What:** an optional last token on `limit`: `gtc` (default, so M1 scripts don't change),
  `ioc`, `fok`, `post`.
- **IOC:** match now, then `cancelled <id> <rest>`. It never rests, just like a market order with a price limit.
- **FOK:** a read-only pre-scan (`can_fill`) sums the opposite side's quantity at prices that cross
  the limit. If that's short, the order is `accepted` and then `cancelled` whole, with no trades.
  Otherwise it matches normally and is guaranteed to fill completely.
  - **Alternative:** match, then roll back. That's harder to get right and would emit trades that later "didn't happen".
  - **Accepted, then cancelled, not rejected:** it was a valid order that the market couldn't
    satisfy. `rejected` means the order itself was invalid.
- **Post-only:** rejected `would-cross` if it would trade on arrival, so its id stays free.
  The flag is remembered on the resting order, so a later modify can't turn it into a taker.
  - **Alternative:** "slide": reprice it one tick behind the touch. That's common on crypto venues, but it means the engine picks prices for the client.
- **Self-trade prevention:** moved to Tier 3. It needs an owner/account field on every order, which is a format change best done together with a binary gateway protocol.

### D13: Conservation ledger (M2)
- **What:** `Ledger` rebuilds every live order's open quantity from the commands and events
  alone, then after each command compares the total, and the number of live orders, with the
  book's public depth. It also checks each event is legal:
  - trades only between open orders, never above an order's open quantity, and never through the taker's limit
  - cancels report exactly the open quantity
  - market, IOC and FOK orders are done by the end of their own command
- **Why:** the invariant checker looks *inside* one book; the ledger checks the *output* contract,
  which is what M4's fast book must match and what M3's replay records. A book that loses or
  invents one unit of quantity fails it.
- **Found on its first run:** a bug in the ledger itself. An IOC rejected as a duplicate shares
  its id with an older order that's legitimately resting, so "must be done" applies only to
  accepted orders.
- **Randomized test:** 30 seeds × 5,000 commands, with narrow prices, small quantities, and some
  zero, oversized and off-tick values and reused ids. The ledger and invariants are checked after
  every command, and the test asserts a minimum trade count so a broken generator can't pass
  silently. A hand-written SplitMix64 (`rng.rs`) keeps seeds stable forever.

### D14: Instrument config (M2)
- **What:** `BookConfig { tick_size, max_qty }`, defaults 1 and 1,000,000, enforced on new
  orders and modifies (`bad-tick`, `qty-too-large`). Check order: zero, too large, tick, then id
  (and for post-only, would-cross). Only one reason is reported, so the order is part of the contract.
- **Why in the engine:** the tick grid is a property of the book (levels must sit on it, and
  the invariant checker verifies they do). Max qty is the classic fat-finger guard
  (Knight Capital and others). Broader pre-trade risk (position and notional limits) is Tier 3.
- **Scenario support:** a `config <tick> <max>` line starts a fresh book under those rules
  (`OrderBook::with_config`), so every book implementation can be built the same way.
- **Mutation-checked (M2):** 12 planted bugs, all caught:
  - always keeping priority on a modify, or misreporting an in-place modify's quantity
  - no FOK pre-scan, or a pre-scan that ignores the limit
  - IOC orders resting
  - no tick check, or no max-qty check
  - the post-only check skipped on arrival or on a modify, or the post-only flag lost on rest
  - the flag lost after a modify moved the order. This one first survived, and a new scenario now covers it.

### D15: Command journal format (M3)
- **What:** an 8-byte header (`LOBJ`, version u32), then records `[crc32][len u16][payload]`. The CRC
  covers the length and the payload. Payloads are a tag byte plus fixed-width little-endian fields
  (27 bytes for a limit). Full layout is in `src/journal.rs`.
- **Why binary:** about 27 bytes per command (20,000 commands is 540 KB), and decoding is a few
  loads with no parsing, so replaying millions of commands is dominated by matching, not I/O.
  The text format (D6) stays for humans.
- **Why the CRC covers the length:** a flipped length would otherwise send the reader to a random
  offset with nothing to catch it (the same lesson as lsmkv's D1).
- **Version field:** a future format (an owner field for self-trade prevention, timestamps) can
  coexist with old recordings instead of silently misreading them.
- **Not a WAL:** the journal is a recording of input, written with a `BufWriter` and never fsynced
  per record. Crash-safe journaling for recovery is Tier 3, and lsmkv's group commit is the model for it.

### D16: Damaged journals
- **Torn tail** (the file ends inside a record, or the *last* record fails its CRC): replay the
  complete records and report `torn_tail` with the offset. That's what a crash mid-write leaves.
- **A bad record followed by more data:** `Corrupt(offset)`, refuse to replay. A crash can only
  tear the end, so damage in the middle means the file is bad, and replaying part of a session
  would produce a plausible but wrong book. It's the same rule as lsmkv's D2.
- **A valid CRC around an undecodable payload** (unknown tag, bad side or TIF byte, wrong length):
  `InvalidRecord`. That's a bug or a version mismatch, not disk damage.
- **Known limit:** a damaged length that points past EOF is indistinguishable from a torn
  write. Only a header checksum would separate them (the same limit as lsmkv's D2).
- **Tests:** every truncation point, and every byte of every non-last record flipped.
  A flipped CRC or payload byte must be `Corrupt`; only a flipped length byte may read as torn.

### D17: Replay output and digest
- **What:** each event becomes `seq u64 | tag | fixed-width fields`, with sequence numbers starting at 1
  and counting every event in the session. The digest is FNV-1a 64 over exactly those bytes, and
  `lob replay <journal> <events-file>` writes them out.
- **Why sequence numbers:** downstream consumers (market data in M8, a replica) detect a gap or a
  duplicate by the number alone, which is how exchange feeds work (ITCH, MoldUDP64).
- **Why FNV-1a:** hand-written in 10 lines and frozen, so a digest recorded today stays valid. It
  isn't cryptographic: it detects accidental divergence, not tampering. Checked against published
  test vectors.
- **Golden digest:** seed 1 × 20,000 commands gives `f0cd0c4be21b0c27` (28,834 events,
  8,316 trades, 5,519 rejects), pinned in `tests/replay.rs`. A deliberate change to matching
  updates it here with a reason; an accidental one is a bug.
  The same digest comes out of a debug test build and the release CLI.
- **Proof of determinism:**
  - two runs give byte-identical streams
  - replaying from a journal gives the same result as replaying from memory
  - a different seed, or one dropped command, changes the digest

### D18: Synthetic order flow
- **What:** a seeded generator with a random-walk mid (10% of steps move one tick). The mix:
  - 48% passive limits, 1–12 ticks from the mid and skewed to the touch, about 10% of them post-only
  - 8% aggressive limits up to 3 ticks through the mid (half IOC, a quarter FOK, a quarter GTC)
  - 4% market orders
  - 30% cancels and 10% modifies of orders it believes are live (half reduce in place, half move the price)
  - Lot sizes 1–500, mostly small. It tracks at most 5,000 live orders.
- **Shape:** for seed 1 the book settles around 700 resting orders over about 90 levels. 28% of commands are
  rejected, almost all late cancels and modifies of orders that already filled: the generator
  can't see the book, the same as a real client with fills in flight.
- **Alternatives:** uniform random prices (no queue build-up at the touch, so matching is rarely
  exercised), or real data (M7, ITCH).
- **Mutation-checked (M3):** 10 planted bugs, all caught after two test fixes:
  - mid-file damage read as a torn tail, or a torn tail read as corruption
  - trailing payload bytes accepted
  - side bytes swapped, the CRC skipping the length, or the version not checked
  - sequence numbers not counted, the final chunk left out of the hash, the FNV prime changed, or reject codes swapped
  - Survivors at first: "mid-file damage read as torn" (the bit-flip test was too lenient) and
    "trailing bytes accepted" (no test).

### Build profile
- `[profile.dev] opt-level = 1`: the randomized and replay tests run O(book) checks after every
  command. Unoptimized, the suite took about 30 s; now it's about 2 s. Debug info and overflow checks stay on.

### D19: Slab storage for orders and levels (M4)
- **What:** `Slab<T>` is a `Vec<T>` plus a free list of indices. Orders and price levels live in
  slabs and refer to each other by `u32` index. A removed slot is reused by the next insert.
- **Why:**
  - Once the book is warm, resting an order reuses a slot instead of allocating. M6 will prove zero
    allocations per command with a counting allocator.
  - Nodes are contiguous, so walking a queue touches nearby memory more often than `Box`ed nodes scattered over the heap.
  - `u32` indices are half the size of pointers and need no `unsafe`.
- **Cost:** a stale index after removal would silently point at a reused slot (the slab
  version of use-after-free). `check_invariants` walks every list and checks back links, level
  ownership, the index and the slab's live count, and the differential test runs it constantly.
  A generation counter per slot is the standard fix if this ever bites.

### D20: Intrusive doubly linked list per level, with aggregates
- **What:** each order node holds `prev`/`next` slot indices. Each level holds head, tail, total
  quantity and order count.
- **Why:** O(1) append at the tail, O(1) fill from the head, and O(1) removal from anywhere given
  the slot. The reference book's `VecDeque` needs O(n) to remove from the middle.
  `total` makes depth queries and the FOK pre-scan walk levels instead of orders.
- **Subtlety:** when a fill takes a maker to zero, the fill has already come out of the level
  total, so `unlink` (which subtracts the node's remaining quantity, now 0) doesn't subtract it
  twice. A mutation dropping either subtraction was caught.

### D21: Id index and best-level cache
- **What:** `HashMap<OrderId, slot>` for cancel and modify. `best: [u32; 2]` caches each side's best
  level. It's updated when a better level is created, and recomputed from the price tree only
  when the best level empties.
- **Why:** matching asks "what's the best level?" for every level it sweeps. The cache makes that
  a load instead of a tree search. The tree stays the source of truth (a tick-indexed array is M6).
- **Determinism:** `HashMap` uses a random hash seed per process, but the engine only looks up
  and removes by key and never iterates it, so output can't depend on the seed (D4).

### D22: Differential testing and measured results (M4)
- **Proof of equivalence:** the fast book must produce *identical events* to the reference book
  for every command:
  - all 11 scenario files, run against both books
  - 5 generated sessions × 40,000 commands, with depth, both books' invariants and the ledger checked every 10 commands
  - 30 edge-case sessions × 10,000 commands, checked after every command
  - a 3,000-order deep-queue worst case
  - **Release run** (`cargo test --release --test differential -- --ignored`): 10 generated sessions
    × 1M plus 10 edge-case sessions × 500k, 15M commands in total, all identical (49 s).
  - It matched on the first run, after the code was written against the reference.
- **Mutation-checked:** 12 planted bugs, all caught:
  - a stale best cache, or the best-level comparison inverted
  - level totals not maintained on fill, rest, or an in-place modify
  - FOK summing order counts instead of quantity
  - filled orders left in the index, or the post-only flag lost on a modify
  - no slot reuse, or a modify that always keeps priority
  - broken tail or prev links
- **Throughput** (`lob bench`, release, best of 5, one core; "apply" excludes event encoding and hashing):

  | Workload | Reference | Fast | Speedup |
  |---|---|---|---|
  | Generated flow, 2M commands (~1,700 resting) | 13.6 M/s | 16.1 M/s | 1.19x |
  | Generated, deeper (max-live 200k, ~17,000 resting) | 13.0 M/s | 14.3 M/s | 1.11x |
  | One queue of 1,000 orders, random cancels | 7.8 M/s | 15.7 M/s | 2.0x |
  | One queue of 10,000 | 1.5 M/s | 18.6 M/s | 12x |
  | One queue of 50,000 | 0.30 M/s | 15.2 M/s | **51x** |

- **Reading the numbers honestly:**
  - On realistic flow, prices spread over many short levels, so the reference book's O(level) scan
    is cheap, and both books spend most of their time on the same things: two SipHash lookups per
    order (`used` and the index), the `BTreeMap` price tree, and pushing events.
  - The fast book's structure buys a *bound*: cancel costs the same however deep the queue, while
    the reference book degrades quadratically. Tail latency (M5) is where that shows on real flow.
  - The next speedups are the shared costs: a cheaper hasher or dense ids, a tick-indexed ladder
    instead of the tree, and no allocation (M6). The numbers above are the baseline that work is measured against.

### D23: Timing lives in a harness, never in the engine (M5)
- **What:** `src/latency.rs` and `lob latency <journal> [runs]` read a clock around each `apply`
  call. The books never read a clock (D4), so replay and the golden digest are unaffected.
- **Alternatives:** timestamps inside the books (breaks D4: output would depend on the clock).
- **Why:** real engines put timestamps on events at the gateway or sequencer, outside the matching
  logic, for the same reason.

### D24: HdrHistogram, 3 significant digits, nanoseconds
- **What:** one `hdrhistogram` histogram per command kind, one for all commands, and one for the
  clock floor. The range is 0 ns to 10 s, and every value is kept to within 0.1%.
- **Alternatives:** keep every sample and sort it (2M × 8 bytes per run, fine here but not in a live
  engine), or hand-written log buckets.
- **Why:** constant memory, O(1) recording, and exact enough to read p99.9 and max. It's the
  standard tool in trading and in latency benchmarks such as wrk2.

### D25: `Instant::now()` around each `apply`, and its cost reported
- **What:** only `apply` sits between the two clock reads. Clearing the buffer, classifying and
  recording all happen outside the timed window. The same number of empty windows are timed
  in the same run and printed as "clock floor".
- **Measured floor:** 14–15 ns p50 on this machine when idle. That floor is included in every sample, so a
  50 ns p50 is really about 35 ns of work.
- **Alternatives:** `rdtsc`, which this CPU supports (`constant_tsc`, `nonstop_tsc`). It's cheaper,
  but needs calibrating to nanoseconds and a fence to stop out-of-order execution moving it.
  Linux's vDSO `clock_gettime` already reads the TSC with that ordering.

### D26: A command's kind is what it did, not just what it was
- **What:** seven histograms: limit-rest, limit-cross, limit-kill (an IOC/FOK that traded nothing),
  market, cancel, modify and reject. `Kind::of` decides from the events after `apply`. A
  single `rejected` event means reject. Otherwise a limit with a trade is a cross, a limit with
  a cancel is a kill, and anything else rests.
- **Why:** one overall number hides the tail. About 37% of generated commands are cheap rejects,
  which pull the overall p50 down. A limit that sweeps several levels and one that rests run
  different code. A modify that then trades is still a modify, because that's what the client sent.

### D27: Method: warm-up, pinned core, median of 5 runs, machine printed
- **What:** `scripts/latency.sh [cpu] [runs]` builds release, generates three seeded journals and
  runs `taskset -c <cpu> lob latency`. Each book gets one untimed warm-up pass, then N runs on
  fresh books, **alternating books** (ref, fast, ref, fast, ...). The run printed is the one with the median overall p99; the other runs' p99s are
  printed too, each with its clock floor (`p99/floor`), so the spread is visible. The output starts with the CPU, kernel, governor,
  turbo setting and the CPUs the process may run on.
- **Why the median run, not the median of each column:** every number in a table then comes from one real run.
- **What isn't controlled:** the governor (`powersave` with the `balance_performance` hint; changing it needs root), turbo, and the
  SMT sibling (cpu 6 shares cpu 2's core).
- **Why alternate:** the first version ran all 5 reference runs and then all 5 fast runs. The clock
  floor, which should be constant, moved between 14 and 23 ns across those runs: the CPU's
  frequency was changing under the `powersave` governor. In one run the reference book had a 23 ns floor and the fast book 15 ns, so
  the comparison was unfair, and it showed up as a fake "fast book has a worse p99.9 on crossing
  limits" (2.4 µs vs 0.8 µs). Alternating removed it (933 vs 917 ns). The floor is also a free frequency
  gauge: compare only runs whose floors match.
- **Lesson learned while building it:** another project's `cargo build` running on the same laptop made every
  percentile 2–3x worse and the tails 5–10x worse. Results are only taken when the load average is low, and
  the run-to-run p99 spread printed with each table is how to tell a quiet run from a noisy one.

### D28: Criterion microbenchmarks at fixed depths
- **What:** `benches/book.rs` has add, cancel and match for both books at 10, 1,000 and 100,000 resting
  orders, spread over 50 levels per side. Ops are timed in chunks of up to 100 and then undone untimed (adds
  cancelled, cancels and fills replaced), so the depth stays within `depth + 100`. After each
  chunk an untimed assert checks the last op rested, cancelled or traded as claimed. Two
  planted fixture bugs, a non-crossing taker and cancelling an id twice, both trip it.
- **Why chunks:** timing each op alone would add the about 15 ns clock floor to an op of about 30 ns.
- **Caveat:** "add" also inserts a fresh id into the ever-growing used-id set, so it includes that
  set's amortized growth. That's honest, because the real engine pays it too, but it's an M6 target.

### M5 results (release, `taskset -c 2`, interleaved, median of 5 runs, every floor 13–15 ns)
Machine: i7-1165G7 laptop, kernel 7.2.5, `powersave` governor (`balance_performance`), turbo on.
All values in ns and include the about 14 ns clock floor. Raw output: `scripts/latency.sh`.

**Generated flow, 2M commands (about 1,700 resting):**

| Kind | Count | Ref p50 | Ref p99 | Ref p99.9 | Fast p50 | Fast p99 | Fast p99.9 |
|---|---|---|---|---|---|---|---|
| limit-rest | 564,749 | 91 | 262 | 731 | 90 | 224 | 765 |
| limit-cross | 303,074 | 64 | 380 | 723 | 61 | 469 | 851 |
| limit-kill | 42,726 | 64 | 184 | 693 | 56 | 175 | 695 |
| market | 66,841 | 60 | 237 | 714 | 55 | 263 | 700 |
| cancel | 231,221 | 106 | 179 | 343 | **49** | **106** | **214** |
| modify | 46,291 | 127 | 342 | 707 | **62** | **188** | **434** |
| reject | 745,098 | 33 | 68 | 149 | 31 | 68 | 163 |
| all | 2,000,000 | 71 | 261 | 630 | 52 | 259 | 685 |
| max | | | | 8.4 ms | | | 8.5 ms |

A second clean run agreed (fast vs ref cancel p99 112 vs 202, cross p99 505 vs 470).

**Deeper book (max-live 200k, about 17,000 resting), cancel and modify are where the structure shows:**

| Kind | Ref p50 | Ref p99 | Ref p99.9 | Fast p50 | Fast p99 | Fast p99.9 |
|---|---|---|---|---|---|---|
| limit-rest | 99 | 392 | 970 | 100 | 290 | 774 |
| limit-cross | 66 | 580 | 1,230 | 63 | 691 | 1,605 |
| cancel | 164 | 687 | 1,544 | 69 | 328 | 860 |
| modify | 219 | 728 | 1,739 | 97 | 398 | 915 |
| all | 62 | 413 | 941 | 58 | 404 | 1,001 |

**One 10,000-order queue, cancelled in random order:** cancel p50 / p99 / p99.9 = 1,086 / 3,557 / 4,987 for the reference book
vs 44 / 61 / 144 for the fast book (58x at p99).

**Criterion** (`benches/book.rs`, mean ns per op, min–max over 3 runs, see the caveat):

| Op @ depth | Reference | Fast |
|---|---|---|
| add @ 10 | 82–102 | 94–97 |
| add @ 100k | 141–231 | 114–126 |
| cancel @ 10 | 55–119 | 41–54 |
| cancel @ 1k | 72–87 | 27–61 |
| cancel @ 100k | **487–703** | **67–136** |
| match @ 10 | 90–173 | 87–99 |
| match @ 100k | 90–247 | 85–97 |

- **Caveat:** another project's benchmark ran during parts of two of the three criterion runs, so the ranges
  are wide. Criterion can't interleave the books the way `lob latency` does. Only the claims that held in
  every run are made below.

**What the numbers say:**
1. **Cancel and modify are the fast book's wins, and they grow with queue depth:** cancel 2.2x at p50 and 1.7x at p99 on
   realistic flow, 2x at p99 on the deeper book, and 58x at p99 on one deep queue. Criterion agrees: at 100k orders the
   fast book is 4–9x faster in every run. That's D22's O(level) vs O(1), now visible in the tail.
2. **Rest, cross and market cost the same in both books** at p50 (within about 5 ns). Both pay
   the same shared costs: SipHash lookups, the `BTreeMap` price tree, pushing events.
3. **The fast book's crossing limits have a slightly worse tail** (p99 469 vs 380 on generated flow, 691 vs 580
   on the deeper book; p99.9 851 vs 723 and 1,605 vs 1,230). This is consistent across every interleaved run, so it isn't noise. D29 shows it
   comes from crosses that fill many makers: each extra maker costs the fast book more.
4. **The max (2–15 ms) is the same in both books, and it's the used-id set rehashing.** Commands over 50 µs
   occur at order ids of about 3.6k, 7.3k, 14.6k ... 941k, each double the last. Those are the points where hashbrown's table fills to 7/8 and
   grows. The set of every id ever used never shrinks, so each resize rehashes all of it inside
   one command: 20 ms at 941k ids. **This is M6's first target**, for example with dense ids or a
   preallocated or bounded structure. A real exchange can't pause one order for 20 ms.
5. **About 37% of commands are rejects at about 35 ns**, which is why the overall p50 (58–78 ns) is lower
   than the p50 of any real operation. Per-kind histograms (D26) keep that from hiding anything.

### D29: Profiling with `perf` (M5)
- **What:** `scripts/profile.sh [cpu] [journal]`:
  - `perf stat` counts user-space events for `lob run <book>`, which applies the journal 5 times with
    nothing else in the loop. `lob run none` only decodes the journal and is subtracted.
  - Then `perf record --call-graph dwarf` on a `profiling` build (release plus debug info) samples where the time goes.
  - `kernel.perf_event_paranoid = 2` allows all of this for a normal user, as long as it's limited to their own process in user space.
- **Counters per command** (generated flow, 2M × 5, pinned, `perf stat -r 3`). Spread within a run was 0.1–2%; the
  reference book's cycles moved about 10% between invocations:

  | | cycles | instructions | IPC | branch misses | L1d misses | LLC misses |
  |---|---|---|---|---|---|---|
  | reference | 329–365 | 660 | 1.8–2.0 | 2.9 | 5.5–5.7 | 1.0 |
  | fast | 277–281 | 596 | 2.1 | 2.1 | 4.0 | 0.9 |
  | reference, deeper book | 343 | 678 | 2.0 | 2.4 | 8.0 | 1.6 |
  | fast, deeper book | 303 | 615 | 2.0 | 2.0 | 4.9 | 1.3 |

  The fast book runs 10% fewer instructions with 25–40% fewer L1 misses. IPC is about 2 for both, so neither is stalled on memory overall.
- **Where the time goes** (fast book, self time, share of all samples including about 10% journal decoding):

  | Function | % |
  |---|---|
  | used-id set `contains_key` (the duplicate-id check) | 18.9 |
  | used-id set `reserve_rehash` (growth) | 11.2 |
  | id index `remove` (fill or cancel) | 11.0 |
  | used-id set `insert` | 6.8 |
  | id index `insert` (rest) | 5.9 |
  | **hashing, total** | **53.8** |
  | book logic: `apply`, `rest`, `submit`, `take`, `unlink` | 30.2 |

  The reference book's hashing total is 45% (its queues cost more, so hashing is a smaller share).
- **M6 targets, in order:**
  1. **The used-id set** (37% of the fast book's samples): one lookup and one insert for every new order, a table that
     never shrinks and misses the cache at about 1M entries, and a rehash at every doubling (the multi-ms max in the M5 results). Options for the M6 consult:
     dense or monotonic ids (a bitmap, or "ids must increase", which is what many venues require per session), a cheaper hasher, or
     preallocation.
  2. **The id index** (17%): SipHash on a `u64` key is overkill. A cheaper hasher, or a direct-mapped
     table if ids are dense.
  3. **Then the price tree and the order struct layout**, which only become visible once hashing is gone.
- **The crossing-limit tail (M5 result 3), explained in part:** a scratch run grouped crossing limits by how many makers
  they filled (books interleaved, run 3 of 5, generated flow):

  | Makers filled | Count | Ref p50 / p99 | Fast p50 / p99 |
  |---|---|---|---|
  | 1 | 225,080 | 89 / 434 | 90 / 402 |
  | 2–3 | 41,523 | 154 / 551 | 159 / 546 |
  | 4–7 | 23,273 | 237 / 731 | 266 / 713 |
  | 8+ | 13,198 | 423 / 1,133 | 502 / 1,269 |

  The gap only appears when a cross fills several makers, and it grows with the number filled, so each extra maker
  costs the fast book more. There are two possible causes, and this run doesn't separate them:
  - **memory:** each maker is a slab node at a scattered index, while the reference book's queue is a contiguous `VecDeque`
  - **work:** `unlink` fixes neighbour links, updates the level totals and pushes to the free list, versus one `pop_front`

  M6 can tell them apart with `perf annotate` on `take`/`unlink`, or by filling orders without unlinking them one at a time.
- **Not done:** an SVG flamegraph (it needs `inferno` or the FlameGraph scripts, which aren't installed). `perf report`'s
  text output answered the questions above.
