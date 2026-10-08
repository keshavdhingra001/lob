# lob design

Every non-obvious decision has an entry below: **what**, **alternatives**, **why**. The entries are in the order the decisions were made (D58),
so later ones sometimes replace earlier ones, and each replaced entry says what replaced it. Numbers come from dated results sections and
[BENCHMARKS.md](BENCHMARKS.md), each tied to raw output in `bench/results/`.

## Overview (as built)

```
 journal file ──> gateway thread ──SPSC ring──> matching thread ──SPSC ring──> output thread
 (D15; ITCH       decode, stamp                 fast book: apply()             encode + digest (D17)
  via D54)        (D46, D48)                    Command in, Events out         L2 feed publisher (D40)
                                                no clock, no I/O (D4)          end-to-end latency (D48)
```

- **The engine** is one call, `OrderBook::apply(&Command, &mut Vec<Event>)`: deterministic (D4), with no allocation once warm (D5, D32).
  It has two implementations that must emit identical events. The **reference book** (D8) is a `BTreeMap` of `VecDeque`s, written to be
  obviously correct. The **fast book** (D19–D21, D30–D34) uses a slab of orders, an intrusive list per level, a tick-indexed price ladder
  with a bitmap, and O(1) cancel.
- **Matching:** price-time priority, trades at the maker's price (D9), modify with exchange priority rules (D11), IOC / FOK / post-only (D12),
  tick and max-quantity checks (D14), self-trade prevention by STP group (D67–D73).
- **Input and output:** a checksummed binary command journal (D15, D16); a sequence-numbered event stream with a 64-bit digest (D17);
  an L2 feed with gap recovery (D40–D44); a three-thread pipeline whose output equals one thread's byte for byte (D45–D47).
- **Crash recovery:** a live engine with group commit (D78) and atomic snapshots of the logical book (D74–D77). Recovery replays the journal
  after the snapshot, so the recovered digest equals the uninterrupted run's (D79, D81).
- **How it's checked:** scenario scripts and an invariant checker (D10), a conservation ledger built from events alone (D13), differential
  testing of the two books over 15M commands (D22), property tests (D49–D52), mutation checks of every milestone, a real NASDAQ day
  replayed with zero errors (D37, D38), and our matching compared with NASDAQ's on real executions (D54).
- **How it's measured:** a timing harness outside the engine (D23–D27), criterion (D28), `perf` (D29), coordinated-omission-safe pipeline
  latency (D48), and one reproducible report (D53–D57) with percentile plots (D60).

## Decisions by topic

| Topic | Entries |
|---|---|
| Core model: prices, ids, determinism, output buffer, text format | D2–D6 |
| Matching rules and order types | D9, D11, D12, D14, D30 |
| Self-trade prevention | D67–D73, M13 results |
| Reference book | D8 |
| Fast book | D19–D21, D31, D33, D34 |
| No allocation on the hot path | D5, D32 |
| Journal, replay, digest | D15–D17 |
| Crash recovery: snapshots, group commit | D74–D81, M14 results |
| Test oracles and generated flow | D10, D13, D18, D22, D49–D52, D64–D66 |
| Latency and profiling | D23–D29, D48, D55, D60 |
| Real market data (NASDAQ ITCH 5.0) | D35–D39, D54 |
| Market data out | D40–D44 |
| Threads | D7, D45–D47 |
| Reporting and this document | D53, D56–D59, D61–D63 |

**Common questions, and where they're answered:**
- Why integer prices? D2. Why do clients pick order ids, and why must they increase? D3, D30.
- How is cancel O(1)? D20, D21, then D33 for the price ladder.
- How do you know the fast book is right? D22 (differential testing), D50 (properties), D10 and D13 (oracles).
- Why is matching single-threaded, and what did three threads buy? D7, D46, M9 results (nothing at full load: the output stage dominates).
- What makes replay deterministic, given a randomly seeded `HashMap`? D4, D17, D21.
- How is tail latency measured honestly? D25 (the clock's own cost), D27 (method), D48 (coordinated omission).
- Where does the time go? D29, M5 and M6 results.
- Does the matching agree with a real exchange? D54, M11 results.

## Not built (D61)

What a production exchange has that this engine doesn't, and where each would go:
- **A network gateway and wire protocol** (FIX, or a binary protocol like Nasdaq's OUCH, over TCP). Input today is a journal file. A socket reader
  would replace the journal decoder in the gateway thread (D46). Every latency here is in-process: no network, no kernel bypass.
- **More than one symbol.** One book per symbol, symbols sharded across matching threads, a ring per shard (D7). Built for one symbol only.
- **Auctions** (the opening and closing cross). ITCH cross executions only remove shares (D54).
- **Hidden, iceberg, pegged and stop orders.** The FOK pre-scan (D12) is correct only because all quantity is visible.
- **Decrement-and-cancel self-trade prevention** (CME's fourth mode; D68). The three other modes are built (D67–D73).
- **Pre-trade risk** beyond the fat-finger quantity limit: position and notional limits (D14).
- **Replicas.** Crash recovery is built (D74–D81), but there's one copy: no standby replaying the journal over the network, and no failover.
- **Arbitrary client ids.** Ids must increase per session (D30). A gateway would map client ids to internal increasing ones.
- **Timestamps on events.** The engine never reads a clock (D4). A gateway would put a time inside each command, and events would carry it.
- **Entry-time priority for orders displayed late**, which NASDAQ has (M11 results). The engine only sees arrival order.
- **Per-thread core pinning** (D46; `taskset` pins the process), and an exhaustive check of the ring's memory orderings (loom). Miri checks them on sampled interleavings (D64).

## Future work (D63)

- Top-N or incremental snapshots: full-depth recovery moved 1.14 GB for a 42 MB AAPL feed (M11 results).
- Journal segments deleted once a snapshot covers them (D80); snapshots written off the matching thread (D77).
- A power-loss test of the fsync points (D81), e.g. on a fault-injecting file system.
- `perf stat` on the fast book's cancel p99 in the deep200k journal (BENCHMARKS.md).
- The ITCH runs with a controlled page cache (M7's throughput depends on whether the file is cached).
- Longer fuzzing runs on a quiet machine, with seed corpora from the scenario files and the generator (D66).
- loom for the ring: every interleaving instead of Miri's sampled ones (D64).

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
  Uniqueness rules: D9, replaced in M6 by "ids must increase" (D30).

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
  state allocates nothing, proved in M6 with a counting allocator (D32).

### D6: Text command format (for the REPL and tests)
- **What:** one command per line: `limit <id> <side> <qty> <price>`, `market <id> <side> <qty>`,
  `cancel <id>`. `Display` prints exactly what `FromStr` parses (round-trip tested). Events print
  one line each, for scenario tests to compare against.
- **Why:** readable scenario files and REPL sessions. The binary journal format for replay is a
  separate decision in M3, where size and decode speed matter.

### D7: Single-threaded matching per symbol (built in M9 as D46)
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
  - A sorted `Vec` of levels: O(P) inserts. A tick-indexed array is what M6 built (D33).

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
  grows with the session. **Superseded in M6 by D30** (ids must increase), after M5 measured the
  set at 37% of the fast book's time and as the cause of its multi-ms worst case.
- **Added later:** modify, IOC/FOK/post-only and tick size in M2 (D11, D12, D14). Self-trade prevention is not built (see Not built).

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
  doesn't know about a fill yet). Open quantity is simpler and explicit; a gateway (not built) would
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
- **Self-trade prevention:** not built (see Not built). It needs an owner/account field on every order, which is a format change best done together with a binary gateway protocol.

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
  (Knight Capital and others). Broader pre-trade risk (position and notional limits) is not built.
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
  per record. Crash-safe journaling for recovery is not built, and lsmkv's group commit is the model for it.

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
  - Once the book is warm, resting an order reuses a slot instead of allocating. M6 proved zero
    allocations per command with a counting allocator (D32).
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
  a load instead of a tree search. The tree stays the source of truth (until M6 replaced it with a tick-indexed ladder, D33).
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
- **Throughput** (2026-10-04, `lob bench`, release, best of 5, one core; "apply" excludes event encoding and hashing):

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

### D30: Order ids must increase within a session (M6)
- **What:** a new order's id must be greater than the highest id accepted so far this session,
  or it's rejected `id-not-increasing` (this replaces `duplicate-id`, and keeps its event code 4). Both books store
  one `last_id` instead of a set of every id ever used. Gaps are fine. A rejected order
  doesn't raise the bar, so "rejects change nothing" (D9) still holds.
- **Why:** M5/D29 measured the used-id set at 37% of the fast book's samples, and its doubling
  rehash as the 8–20 ms worst case in both books. An increasing id needs O(1) memory and
  one compare, and it still rules out reuse, the case that matters (a late cancel hitting a new order).
- **Real venues:** Nasdaq's OUCH requires each order's `UserRefNum` to be greater than the last one on the session;
  FIX only requires `ClOrdID` to be unique. Clients generate ids from a counter anyway.
- **Cost:** a client that sends ids out of order now gets rejected. A gateway that accepts
  arbitrary client ids could map them to increasing internal ids. That's a gateway job (not built), not the book's.
- **Checked:**
  - The golden digest didn't change, because generated ids always increased.
  - `06_rejects.txt` now also covers a gap being accepted, a lower never-used id being rejected, and a rejected id not raising the bar.
  - 6 planted bugs, 3 per book, all caught: `<` instead of `<=`, the bar never raised after the first id, and only even ids checked.

### D31: fmix64 hasher for the fast book's id index
- **What:** `src/hash.rs`: `IdHasher` runs MurmurHash3's 64-bit finalizer ("fmix64": three xor-shifts and
  two multiplies) on the `u64` id, replacing SipHash-1-3. Only the fast book uses it; the
  reference book stays obviously correct and slow on purpose.
- **Why:** after D30 the id index is the largest hashing cost left (17% in D29). SipHash protects against
  attackers choosing colliding keys, which costs a lot for an 8-byte key.
- **First attempt, rejected by its own test:** a single 128-bit "folded" multiply (the high and low
  halves XORed) left ids that differ only in high bits clustered. `i << 40` for 4,096 ids hit 1,755 of 4,096
  buckets, against about 2,590 for a random hash. fmix64 gets about 2,590 on every pattern tested.
- **Tests:** bucket spread and 7-bit tag spread for sequential, strided and high-bit patterns, plus
  an avalanche test (every input bit flips each bucket and tag bit 40–60% of the time).
  5 planted bugs, all caught. One survived at first: dropping the last xor-shift. An odd multiply
  only reorders the low bits, so bucket counts can't see it; only the avalanche test does (98% vs 56% worst flip rate).
- **Trade-off:** the constants are fixed, so an attacker who can choose ids could aim for collisions.
  The output never depends on the hash (the index is never iterated, D21), so a random seed
  would cost nothing in determinism. Fixed constants keep the table layout, and so
  the performance, reproducible across runs. Behind an authenticated gateway with increasing ids
  (D30) the risk is small; a per-process seed is a one-line change if it isn't.

### D32: Proof of zero allocations per command
- **What:** `tests/alloc.rs` installs a counting `#[global_allocator]` that wraps `System`. It counts `alloc`, `alloc_zeroed` and
  `realloc` in a thread-local `Cell`, which is `const`-initialised with no destructor, so counting can't itself allocate,
  and parallel tests don't mix their counts.
  - After 10,000 warm-up commands, the fast book must apply the next 990,000 with **zero** allocations.
  - That's checked on three generated sessions, one of them with up to 200,000 live orders.
  - The reference book runs in the same harness and must allocate more than 1,000 times, so the test can't pass because nothing was counted.
- **`FastBook::with_capacity(config, orders)`** reserves the order and level slabs, their free lists and the
  id index at startup, the way exchanges size their pools before the open. `with_config` is
  `with_capacity(config, 0)`. The ladder windows are allocated by each side's first order, during warm-up.
- **What made zero possible:**
  - D30: no set that grows with every id
  - D33: no `BTreeMap` node for each new price level
  - the slab free lists (D19), and caller-owned event buffers (D5)
- **What still allocates, by design:** prices outside the ladder window (the overflow tree), growth past the reserved
  capacity, and `depth()` (it returns a `Vec`, and it's a query, not the matching path).
- **Mutation-checked:** 3 planted bugs, all caught: no reservation (`with_config` in the test), the index not reserved, and a
  `format!` hidden in `rest` (wrapped in `black_box` so the optimizer can't remove the allocation).

### D33: Tick-indexed price ladder (fast book)
- **What:** `src/ladder.rs`, one `Ladder` per side:
  - **The window:** 65,536 tick slots centred on the first price that side sees, each holding a level index or `EMPTY`.
  - **A two-level occupancy bitmap:** 1,024 words plus a 16-word summary, so finding the next non-empty level takes a few word operations however sparse the side is.
  - **An overflow `BTreeMap`** for prices outside the window.
  - **Walks without allocation:** every ordered walk (depth, the FOK pre-scan, the invariant checks) is a visitor callback, so nothing on the hot path allocates.
- **Why:**
  - Inserting a new price level into a `BTreeMap` allocates a node, and removing one frees it. That makes "zero allocations per
    command" (D32) impossible while levels are created and emptied all the time, which they are (D18's random-walk mid).
  - A slot lookup is one array index instead of a tree search.
- **Sizing:** 256 KiB of slots plus 8 KiB of bitmap per side, allocated on that side's first order. Only the slots
  near the touch are ever in cache. ±32,768 ticks covers far more than a day's range for a normal
  instrument (a $100 stock with a 1-cent tick moving 5% is 500 ticks).
- **Not done:** the window never moves. A market that drifts outside it falls back to the tree,
  which is correct but allocates, like M4. The options are re-centring, or a price collar that rejects orders far from a
  reference price, as real venues do (limit up / limit down).
- **Tests:**
  - A randomized test against a `BTreeMap` model, including both window edges, far overflow above and below, tick 5, and negative prices.
  - A new differential flow ("wide") puts 40% of prices at the window edges or far outside, so
    matching crosses between window and tree in both directions. A test confirms it really reaches the overflow.
- **A mutation that ate the machine:** shifting every price by one tick (in `price_at`) made "next level below p" return p
  itself, so `depth` and the invariant checker's walks looped forever, growing a `Vec`. It reached 12 GB, and
  systemd-oomd killed the whole desktop app twice, along with every session running in it.
  - **Fix:** both walks `debug_assert` that each step moves strictly away, so that bug now panics at once.
  - **Mutation runs:** each test run now has a 4 GB `MemoryMax` cap (`systemd-run --user --scope`).
- **Invariant cost:** `Ladder::check` is O(levels + 1,024): the summary against every word, plus every set bit against its slot.
  The full 65,536-slot scan (`check_full`) runs only in the ladder's own tests. Running it after every command made the
  differential suite 142 s instead of 3 s. A stale slot with a clear bit escapes the cheap check, but
  `get` reads slots directly, so it would change events and fail the differential test.
- **Mutation-checked:** 9 planted bugs in the ladder, all caught: bitmap masks, the summary not cleared, a skipped summary word, word-search off by one, price mapping, bit not cleared on remove,
  `len`, and the window-vs-tree choice in both directions.
  In the fast book's use of the ladder: 3 planted bugs (the FOK limit check dropped, `depth(n)` returning n+1 levels, the next best
  searched on the wrong side), all caught after a fix. The `depth(n)` bug survived at first because every test asked for
  unlimited depth; the differential test now compares `depth(side, 0)` and `depth(side, 3)` too. A fourth planted change
  (`return true` past the FOK limit) behaved exactly like the original code, since every later level is past the limit as well, so it was replaced.

### D34: Order and level layout, and the crossing-limit tail
- **What:** `Node` (id, qty, level, prev, next, post-only) and `LevelNode` (price, side, head, tail,
  total, count) are both 32 bytes, two per 64-byte cache line. Compile-time `assert!`s on `size_of`
  make growing either one a deliberate change. No reordering was needed: the 1-byte fields
  already pack into the padding.
- **The crossing-limit tail (M5 result 3, D29), explained:** a scratch experiment timed sweeps that each fill 10 makers from one deep level,
  with the makers' slab slots either *contiguous* (rested into a fresh slab) or *scattered* (the free list shuffled first, as it is in steady state).
  The books alternated, median of 5, on a quiet machine (`taskset -c 2`, M6 code):

  | Makers at one level | Slots | Reference ns per maker | Fast ns per maker |
  |---|---|---|---|
  | 200,000 | contiguous | 32.9 | 32.6 |
  | 200,000 | scattered | 28.3 | **59.6** |
  | 1,000,000 | contiguous | 72.0 | 81.4 |
  | 1,000,000 | scattered | 76.1 | **157.5** |

  When the nodes are contiguous, the fast book's extra bookkeeping per maker (links, level totals, free list) costs nothing at 200k and 13% at 1M.
  When they're scattered, each maker is a cache miss the reference book's contiguous `VecDeque` doesn't pay.
  **So it's memory layout, not work.**
- **Not done (proposed):** prefetch the next maker's node while filling the current one (`_mm_prefetch`; `unsafe`
  and x86-specific), or keep each level's nodes close together (one slab per level, or chunked allocation).
  Both would need the same experiment to prove they help.

### M6 results (2026-10-05, quiet machine: idle 90%, every clock floor 14–15 ns, `taskset -c 2`)
**Fast book, M5 vs M6**, the same journals, the two binaries alternated, 2 runs each (both shown), ns. Each figure is the median of 5 runs.

| Generated flow, 2M | M5 p50 | M5 p99 | M5 max | M6 p50 | M6 p99 | M6 max |
|---|---|---|---|---|---|---|
| limit-rest | 98 / 100 | 264 / 212 | 2.5 / 3.3 ms | **50 / 50** | **95 / 94** | 68 / 125 µs |
| limit-cross | 68 / 67 | 530 / 502 | 5.2 / 6.5 ms | 43 / 44 | 372 / 368 | 126 / 125 µs |
| market | 59 / 61 | 309 / 270 | 10.0 / 13.4 ms | 35 / 35 | 171 / 172 | 44 / 130 µs |
| cancel | 55 / 56 | 125 / 118 | 0.1 / 0.02 ms | 47 / 48 | 71 / 72 | 24 / 25 µs |
| modify | 66 / 66 | 209 / 203 | | 59 / 60 | 151 / 153 | |
| **all** | 57 / 59 | 297 / 266 | **10.0 / 13.4 ms** | **42 / 42** | **186 / 186** | **126 / 130 µs** |

| Deeper book (about 17,000 resting) | M5 p50 | M5 p99 | M5 max | M6 p50 | M6 p99 | M6 max |
|---|---|---|---|---|---|---|
| limit-rest | 103 / 104 | 270 / 275 | 2.6 / 2.4 ms | 54 / 53 | 100 / 93 | 0.23 / 0.25 ms |
| limit-cross | 68 / 71 | 694 / 712 | 9.7 / 10.3 ms | 44 / 43 | 470 / 416 | 0.13 / 0.03 ms |
| cancel | 74 / 74 | 283 / 273 | | 62 / 61 | 207 / 99 | |
| **all** | 63 / 64 | 401 / 406 | **9.7 / 10.3 ms** | **42 / 41** | **256 / 234** | **0.23 / 0.25 ms** |

**Counters** (`scripts/profile.sh`, per command, decode baseline subtracted), M5 (D29) to M6:

| | cycles | instructions | branch misses | L1d misses | LLC misses |
|---|---|---|---|---|---|
| fast | 277–281 to **124** | 596 to **266** | 2.1 to 1.5 | 4.0 to 2.0 | 0.9 to **0.13** |
| reference (it gained D30 too) | 329–365 to 227 | 660 to 419 | 2.9 to 2.8 | 5.6 to 3.3 | 1.0 to 0.13 |

**Where the fast book's time goes now** (self time, including about 20% journal decoding and CRC that isn't matching):
`apply` 17%, id index `remove` 15%, CRC 12%, `take` 11%, `submit` 9%, index `insert` 6%, `rest` 5%,
`unlink` 5%, journal decoding 5%, `Ladder::get` 2%. No `BTreeMap` and no used-id set appear at all.

**What the numbers say:**
1. **The multi-ms worst case is gone:** 10–13 ms down to 0.13–0.25 ms in every run. That was the used-id set's rehash (D30). What's left
   is the slab and index growing in `lob latency`, which uses `with_config` (no reserved capacity), plus OS interrupts.
2. **2.2x fewer cycles per command** (281 to 124) and **7x fewer last-level cache misses** (0.9 to 0.13). Overall p50 is 57 down to 42 ns,
   and about 14 ns of that is the clock floor, so the work itself went from about 43 ns to about 28 ns.
3. **Resting an order is twice as fast** (p50 98 to 50, p99 264 to 95): no used-set insert, a cheaper index insert, and an array slot
   instead of a tree lookup for the level.
4. **The changes aren't measured one at a time.** This A/B compares M5 with all of D30, D31 and D33. The profile shows where
   the time went (the used set gone, SipHash replaced, the tree gone), but not each change's share. Building one binary per decision would give that.
5. **Criterion agrees** (table below): the fast book's add is 3–4x cheaper than in M5 at every depth, because the used-id set is gone.
   The reference book's add halved for the same reason, since it gained D30 as well. That resolves D28's caveat.
6. **The id index is now the biggest single matching cost** (21% for insert plus remove). With increasing ids (D30) a
   direct-mapped table would fit, if ids were dense. Real client ids have gaps, so that's a question for a gateway (not built).

**Criterion, M6** (2026-10-07, `taskset -c 2 cargo bench --bench book -- --warm-up-time 1 --measurement-time 3`, mean ns per op,
min–max over the 2 runs that were quiet before and after: idle 93%/84% and 94%/96%). Two more runs overlapped another workload's
GPU profiler (idle fell to 52%), pushed both books up 1.5–2x, and are left out. M5 columns are copied from D28's table, which
had noisy runs, so M5 vs M6 here is a rough guide; the alternated latency A/B above is the careful comparison.

| Op @ depth | Ref M5 | Ref M6 | Fast M5 | Fast M6 |
|---|---|---|---|---|
| add @ 10 | 82–102 | 41–44 | 94–97 | **24–25** |
| add @ 1k | | 56–80 | | 25–28 |
| add @ 100k | 141–231 | 81–86 | 114–126 | **34–37** |
| cancel @ 10 | 55–119 | 56–60 | 41–54 | 30 |
| cancel @ 1k | 72–87 | 74–128 | 27–61 | 22–24 |
| cancel @ 100k | 487–703 | 437–477 | 67–136 | **39–98** |
| match @ 10 | 90–173 | 38 | 87–99 | 35–38 |
| match @ 1k | | 33–34 | | 29–30 |
| match @ 100k | 90–247 | 37–39 | 85–97 | 36–61 |

- **Add no longer grows with depth much** (fast 24 to 37 ns from 10 to 100k orders): no used-id set, and a ladder slot instead of a tree search.
- **Cancel at 100k is still 4.5–11x faster than the reference book**, D22's O(1) vs O(level).
- **Fast cancel and match at 100k vary 2x between clean runs** (39 vs 98, 36 vs 61), and nothing else does. A guess, not
  measured: at 100k orders the slab (3.2 MB of 32-byte nodes) and index outgrow L2, so the result depends on where pages land.
  `perf stat` on L2/LLC misses for just that bench would settle it.
- Match costs about the same in both books at 10 and 1k orders (within 10%); crossing one maker is mostly shared work.

### D35: Real data is one NASDAQ sample day (M7)
- **What:** `07302019.NASDAQ_ITCH50.gz` (30 July 2019, 3.66 GB gzipped), the smallest full day on NASDAQ's public
  sample site (emi.nasdaq.com/ITCH). It lives in `data/`, which git ignores. Everything streams from the `.gz`;
  an uncompressed copy is only made to measure decompression's share (D39).
- **Changed during implementation:** D35 first proposed committing a small slice of the real file as a test fixture. It isn't committed,
  because the repo may go public and NASDAQ's redistribution terms for the samples aren't clear. The unit tests build
  their streams with `itch::encode` instead, and the whole-day run is an `#[ignore]` test that skips when the file isn't there.
- **Why one day:** one day is about 300M messages and every kind of event (opening and closing crosses, halts). More days
  would add disk and download time, not new behaviour.

### D36: Zero-copy ITCH 5.0 parser (`src/itch.rs`)
- **What:** `Reader` splits the stream on the 2-byte length prefix and returns each message as a slice of its one
  1 MiB buffer; `decode` reads the fields out of that slice into a `Copy` enum. All 23 types are length-checked
  against their fixed sizes; the 11 the book and checks need are decoded, and the rest are `Body::Other`.
- **Why:** nothing is allocated per message, and nothing is copied except the fields themselves.
  The reader isn't an `Iterator` because each slice borrows the reader's buffer until the next call (a "lending"
  iterator, which `Iterator` can't express).
- **Why a wrong length is fatal:** every type has one fixed length, so a mismatch means the framing slipped, and
  everything after it would decode as garbage. Stopping there with the message index and byte offset is the only safe choice.
- **Tests:** two messages written out byte by byte from the spec's tables (so an offset that's wrong in both
  `encode` and `decode` still fails), round trips for every decoded type with extreme values (including a 48-bit
  timestamp), and a reader fed 1, 2, 3, 7, ... bytes at a time so messages straddle reads and the buffer's end.
- **Dependency:** `flate2` with the `zlib-rs` backend (pure Rust). It was picked over the default `miniz_oxide` for inflate speed on the strength of zlib-rs's own published benchmarks; that wasn't measured here.

### D37: A separate `ItchBook` that replays the exchange's book
- **What:** `ItchBook` applies add / execute / cancel / delete / replace by NASDAQ's order reference number and keeps
  every symbol's visible book. It doesn't match: ITCH reports the trades NASDAQ already made, so running the
  messages through our matching engine would produce trades that never happened.
- **Changed during implementation:** D37 first proposed reusing the M6 ladder. That doesn't fit here. A ladder side is 264 KiB once
  used, and about 8,900 symbols × 2 sides would be about 4.7 GB. It also requires prices on one tick grid, and stocks under $1
  are quoted in $0.0001 steps while the rest use $0.01. So each side is a `BTreeMap<price, (shares, orders)>`, and the
  order index is a `HashMap` with the M6 fmix64 hasher (D31). The M6 structures stay in the matching engine, where
  one symbol and one tick size hold.

### D38: How the rebuilt book is checked
- **Hard errors** (replay stops): an execute / cancel / delete / replace for an order that isn't live, an add or
  replace onto a live reference, a fill or cancel larger than what's left, a message whose locate differs from its
  order's, an add for a locate with no directory entry, a 0-share add.
- **Counted, by phase** (pre-market, market hours, post-market), on symbols in state T only:
  - books left crossed (bid > ask) or locked (bid == ask) after a change
  - `E` executions at the best price on the order's side, or not. ITCH names the order in each execution, so
    price-time priority says every displayed execution should be at the touch.
- **Added during implementation: the cross-unwind window.** The first full run found 21 crossed books, 19 of them on SES at 11:11:36.
  `lob itch ... dump SES` showed why. SES was paused (LULD, state P) at 11:06:36. At 11:11:36.761288980 NASDAQ sent the halt-cross print
  and the state change back to T *with the same timestamp*, and only then the 19 `C` executions that take out the crossed orders.
  So the feed says "trading" for about 1 µs while the auction's executions are still arriving. Each symbol now has an
  `uncrossing` flag, set by any cross print and cleared by its next book message that isn't a `C`. Crossings inside
  that window are counted on their own (`crossed_while_uncrossing`) and not as anomalies. A test replays the SES
  sequence, and 3 planted bugs in the flag are caught.
- **Why not compare with a third-party snapshot:** nobody publishes NASDAQ order-book snapshots for the sample days.
  The closest external check is the closing-cross price (see the M7 results).

### D39: Throughput, measured three ways
`lob itch <file> frame | decode | book` times framing alone, framing plus decoding, and the full rebuild, from the `.gz`
or from an uncompressed copy. Timing is in `main`, outside the parser and book, as with `lob bench`.

### M7 results (2026-10-07, quiet machine: idle ≥95% before and after every run, `taskset -c 2`, i7-1165G7)
**The day:** 282,229,684 messages, 8.66 GB uncompressed (3.66 GB gzipped), 8,849 symbols. By type: A 124.2M, D 120.0M, U 21.3M,
E 7.58M, I 3.72M, X 2.36M, P 1.46M, F 1.30M, L 211k, C 136k, Q 17.7k, and a few thousand Y, H, R.

**Correctness (D38):**
- **No hard errors** in 276,789,789 book messages, and **0 live orders at the end**: every one of the 125.5M orders added was later executed,
  cancelled or deleted, with share counts that add up. Peak live orders: 1,964,977. Peak memory (RSS) 234 MB.
- **All 7,582,422 `E` executions were at the best price on their side**, in every phase; none elsewhere.
- **0 crossed or locked books** on trading symbols outside cross-unwind windows; 21 inside them (19 SES, 2 post-market).
- **AAPL:** opening cross $208.74 (283,525 shares, 09:30:00.17), closing cross **$208.78** (1,073,528 shares, 16:00:00.64). The
  closing cross sets the official close. $208.78 hasn't yet been checked against a published historical close (split-adjusted sites show
  ÷4 prices; a search didn't find the day). At 16:00 the AAPL book was 208.85 bid / 208.89 ask, with 4,711 levels.

**Throughput** (2 rounds each, both shown; the 8.7 GB uncompressed file was probably read largely from the page cache, which held about 8 GB of the 15 GB RAM, so "raw" is near memory speed and not disk speed):

| Mode | Uncompressed | From `.gz` |
|---|---|---|
| frame | 43.3 / 42.6 M msg/s (1.33 GB/s) | 12.0 / 11.9 M msg/s |
| frame + decode | 29.1 / 28.7 M msg/s | 10.8 / 10.7 M msg/s |
| full book rebuild | **3.8 / 3.8 M msg/s** (73 s) | 3.1 / 3.1 M msg/s (91 s) |

**What the numbers say:**
1. **The book, not gzip, is the bottleneck.** D39 predicted gzip would dominate; it doesn't. From the raw file, the rebuild costs
   about 225 ns per message on top of decoding (63.5 s over 282M), while inflating adds about 60 ns (17 s). Framing alone runs at 23 ns per message.
2. **225 ns per message is about 5x the matching engine's 42 ns p50** (M6). That's the price of D37's general structures: a `BTreeMap`
   per side and a 2M-entry `HashMap` (tens of MB of order index plus scattered tree nodes, well beyond the 12 MB LLC), touched in reference order,
   which is effectively random. Not measured further yet: `perf stat` on LLC misses would confirm or refute it. Options: a per-symbol price
   ladder sized by tick (sub-dollar stocks need their own grid), or a dense order table, since references only increase through the day.
3. **Decoding is cheap:** 34 ns per message for frame + decode, against 23 ns for frame only.

### D40: Market data is built from the event stream, outside the engine (M8)
- **What:** a `Publisher` (`src/feed.rs`) reads each command and its events, as the ledger (D13) does, and keeps its own
  aggregated book: one `BTreeMap<Price, Level>` per side plus each resting order's side, price and open quantity.
  It never looks inside a book, so it works the same behind either one.
- **Alternatives:** (a) the books emit level changes themselves. That saves a second pass over the events, but it adds work to the hot path and
  to both books, which must stay independent (D22). (b) Diff `depth()` before and after every command. That's
  obviously correct, but it costs O(book) per command. It's used as the test oracle instead.
- **Why:** market data sits behind the output ring (built in M9, D46), on another thread, and sees only events.
  The engine stays exactly as it is.
- **How a taker becomes resting:** an accepted order (or a modified one, which D11 lets trade) is held as the
  command's *pending taker*. Trades reduce it and its makers. A `cancelled` for it drops it. Whatever is left at the end of the command
  rests at its price. Only then does it count towards a level.

### D41: Absolute, price-keyed level updates, coalesced per command
- **What:** each update carries a level's new total, `(side, price, qty, orders)`, and qty 0 means the level is gone.
  Updates are collected over the whole command. Only levels whose final state differs from their state before the command
  are sent, ordered bids then asks, by price. Trades (price, qty, aggressor side, no order ids) come first, in event order.
  The command's last message is flagged `last`.
- **Alternatives:** deltas ("+30 at 100") are smaller but one lost or doubled message corrupts a level for good,
  while an absolute value repairs itself on the next update. Updates by level index (CME MDP3) save the price, but
  one insert near the top shifts every index below it. One message per event shows states that never existed (a
  modified order resting at a crossing price before its trades).
- **Why:** a consumer only ever sees books the engine actually had between commands, never a half-applied sweep,
  and the feed is minimal: tests check it is exactly the depth diff before and after each command.

### D42: Sequence numbers, gap detection, snapshot recovery
- **What:** every incremental message has a sequence number, starting at 1, with no gaps. A snapshot is the full depth of both
  sides plus the sequence number of the last message it includes. The consumer:
  - live: `seq < next` is a duplicate, ignore it. `seq == next`: apply it. `seq > next`: a gap. It stops trusting its book,
    buffers the message and asks for a snapshot.
  - recovering: buffers everything newer than what it holds. On a snapshot it replaces its book, drops buffered
    messages the snapshot already covers, and applies the rest. If they don't continue from the snapshot (more was lost), it asks again.
  - a late joiner starts in recovering.
- **Alternatives:** a reorder window (wait a little before calling it a gap) is common on UDP feeds. With one in-process
  channel nothing reorders, so it would be untested code. Periodic snapshots on their own channel (CME's market
  recovery feed) suit many consumers. On request suits a test, and the consumer logic is the same.
- **Why:** this is the standard incremental-plus-snapshot pattern. The two cases a naive version gets wrong are messages older than the
  snapshot and a gap inside the buffer.
- **Added during implementation: heartbeats.** The lossy-link test failed at its first run. A command whose only message was dropped
  left the consumer showing the previous book and calling it consistent. Nothing later told it that message 221 existed until 222 arrived.
  A feed can't detect a lost *last* message without being told the current sequence number, so the publisher side sends a heartbeat
  (its last sequence number) after each command, and the consumer treats a heartbeat ahead of it as a gap. Real feeds do the same when
  idle (MoldUDP64 heartbeats). The test's property was sharpened too: a consistent consumer shows the engine's book *as of its own
  sequence number*. That may not be the latest book, but it is never one that didn't exist. Code: `src/consumer.rs` (`Consumer`, and the seeded `Link`).

### D43: Binary wire format
Fixed-width little-endian, like the journal and replay streams (D15, D17):

```text
level     seq u64 | tag 1 | flags u8 | price i64 | qty u64 | orders u32     30 bytes
trade     seq u64 | tag 2 | flags u8 | price i64 | qty u64                   26 bytes
snapshot  seq u64 | tag 3 | bids u32 | asks u32 | (price i64 | qty u64 | orders u32) per level, best first
flags: bit 0 side (0 buy / 1 sell; the aggressor's for a trade), bit 1 last message of the command
```
- **Why:** a decoder knows every message's size from its tag. Decoding rejects unknown tags and flag bits. Tests
  push every message through the bytes, and a golden digest of the generated flow's feed is pinned, so the feed
  is as deterministic as the event stream (D4).
- **Not done:** packet framing (MoldUDP64: session, first sequence number, count) and a real transport. Loss is simulated per message.

### D44: Tests and measurement
- **Oracle:** after every command of random sessions (both books), the publisher's messages must equal the diff of
  `depth()` before and after, and the publisher's book must equal the engine's.
- **Lossy channel:** a seeded channel drops, duplicates and delays messages. Snapshots are taken either when requested
  or later, when delivered. At every batch boundary while live, the consumer's book must equal the engine's.
- **CLI:** `lob feed <journal> [drop-percent] [seed]` prints message and byte counts, the digest, gaps and recoveries,
  and times the publisher against applying the commands alone.
- **Not in M8:** a feed from the ITCH day (one per symbol, from `ItchBook`). It's a follow-up if wanted.

### M8 results (2026-10-07, `lob feed`, 2M generated commands from `lob gen 1 2000000`, quiet machine, `taskset -c 2`, i7-1165G7)
- **Feed size:** 1,249,419 level updates and 694,144 trades: 55.5 MB, **27.8 bytes per command**. Digest `aeb1c067880da1dd`, the same from both books
  (the tests pin a 20k-command digest).
- **Publisher cost**, best of 5, two rounds, alternating with apply-only runs:

  | | apply only | apply + publish + encode | publisher |
  |---|---|---|---|
  | first version (SipHash index, an error string built per command) | 28.8 / 29.0 ns | 118.1 / 118.7 ns | +89 ns |
  | fmix64 index (D31), error strings built only on error | 30.0 / 30.1 ns | 100.0 / 100.1 ns | **+70 ns** |

  `perf record` found the second fix: `ok_or(format!(..))` builds its argument eagerly, so every command with a taker
  formatted an error message it then threw away (`fmt::write` and `format_inner` were about 4% of samples). The ledger had the same pattern
  in three places, now fixed too. The hasher alone was worth about 5 ns.
- **The publisher costs over twice what matching does** (70 ns vs 30 ns). Most of it is the `BTreeMap` per side (`touch` and the
  level lookups at the end) and sorting the touched levels. That's the price of D40's choice to stay outside the engine.
  A ladder like the fast book's (D33) would remove the tree. Having the book report level changes (alternative (a)) would remove the second
  copy of the book altogether. Neither is needed for correctness; both are options if the feed must keep up with the engine on one core.
- **Recovery at 1% loss** (5% duplicates, seed 1): 19,030 gaps, each healed by one snapshot. 105k duplicates ignored. The final book
  matches the engine's. 92.4 MB was delivered, against 58.3 MB with no loss. About 35 MB of that is snapshots, roughly 1.8 KB each (about 90 levels).
  Full-depth snapshots are the expensive part of loss, which is why real feeds send them on a separate channel and let a recovering
  consumer pick one up rather than asking for one per gap.
- **Mutation checks:** 24 planted bugs across `feed.rs` and `consumer.rs`. 4 survived at first: a check that couldn't fire (removed), the
  maker-price check (test added), the link's duplicates (test added), and one equivalent mutant left as it is: not trimming the already
  replayed part of the buffer only wastes memory, since the next snapshot skips those messages anyway.

### D45: A hand-written bounded SPSC ring (M9, `src/ring.rs`)
- **What:** one producer, one consumer, a power-of-two array of slots, and two monotonically increasing indices: `tail`, written only
  by the producer, and `head`, written only by the consumer, each on its own 64-byte cache line. A push writes the slot, then
  publishes `tail` with `Release`. A pop reads `tail` with `Acquire`, so the slot's contents are visible before it reads them. The same holds
  in the other direction for `head`, so a slot is never overwritten while it's still being read. Each side caches the other's index and re-reads the shared one
  only when the cache says full or empty, so in steady flow the two cores rarely touch each other's line.
- **Type-level guarantees:** `Producer` and `Consumer` are separate, non-`Clone` handles, so "single producer, single consumer" is enforced
  by the compiler, not by convention. `T: Copy` means no slot ever needs dropping, which removes the hardest `unsafe` cases
  (a panic mid-push, items left at shutdown). Dropping the `Producer` closes the ring, and the consumer sees `None` once it's drained.
- **Waiting:** spin with `spin_loop()` for a while, then `yield_now()`. A full ring blocks the producer (backpressure); nothing is dropped.
- **Alternatives:** `std::sync::mpsc::sync_channel` (a mutex/condvar-based MPMC design, built for generality) and crossbeam
  (a dependency). The pipeline runs over either the ring or `sync_channel`, so the choice is measured, not asserted.
- **Limit of the tests:** x86 is strongly ordered (TSO). Weakening `Acquire`/`Release` to `Relaxed` still passes every test here,
  because the hardware doesn't reorder these stores. Only a model checker (loom) or a weakly ordered CPU (ARM) would catch it.
  **Update (M12, D64):** Miri catches all six weakenings, so the orderings are now tested, not only argued.
  The orderings are argued in comments, not proven by tests.

### D46: Three threads: gateway → matching → output
- **Gateway:** decodes each command from its 32-byte journal encoding (the "wire"), stamps it, and pushes it.
- **Matching:** pops a command, `apply`s it to the fast book, and pushes each event, then a `Done` marker carrying the command.
  Only this thread touches the book. It never reads a clock: it copies the gateway's stamp through (D4).
- **Output:** gathers each command's events up to `Done`, then encodes and hashes the event stream (D17), runs the publisher (D40), and records
  end-to-end latency.
- **Why fixed-size ring items:** a command makes a variable number of events. Sending them one by one, plus a `Done`, keeps every ring slot `Copy` and small.
  The alternative, a `Vec` per command, allocates on the hot path (D32).
- **Not done:** pinning each thread to a core (needs `libc` or a crate; `taskset` pins the process), a journal written by the gateway, more than one symbol.

### D47: The pipeline must equal the single-threaded run
- **What:** a single-threaded function does the same work in one loop (decode → apply → encode, hash → publish). The pipeline's event digest and feed
  digest must equal it for random flows and for ring capacities 1, 2 and 64. Capacity 1 forces a hand-off on every item.
- **Why:** threads must not change the output. The one ordering that matters, the order of commands into the book, is fixed by
  having one gateway and one matching thread.

### D48: Latency measured from a schedule (coordinated omission)
- **What:** `lob pipeline <journal> [rate] [ring|mpsc]`. At rate 0 the gateway floods: that measures throughput, and latency is mostly
  queueing. At a fixed rate the gateway sends command *i* at `start + i/rate` and stamps it with that *scheduled* time, not
  the time it actually went out. If the pipeline stalls, the commands queued behind the stall are charged for the wait.
- **Why:** stamping with the actual send time hides stalls. A slow pipeline also slows the sender, so fewer samples land in the bad
  period ("coordinated omission", Gil Tene). The schedule fixes it.
- Clock reads happen only in the gateway and output threads (D23). `Instant` is monotonic across cores on Linux (`CLOCK_MONOTONIC`).

### M9 results (2026-10-07, `lob pipeline`, 2M generated commands, quiet machine, `taskset -c 1,2,3` = three physical cores, i7-1165G7)
Each configuration ran 3 times, alternating one thread and the pipeline. All digests were equal in every run: events `b3df3bac1e73d7f6`, feed `aeb1c067880da1dd`.

**Throughput, flooding (ring capacity 1024):**

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| one thread (decode → apply → encode + hash → publish) | 4.96 M/s | 5.04 M/s | 5.02 M/s |
| pipeline over the ring | 3.88 M/s | 3.84 M/s | 3.81 M/s |
| pipeline over `mpsc::sync_channel` | 3.60 M/s | 3.30 M/s | 3.30 M/s |

**Three threads are slower than one at full load.** A pipeline runs at the speed of its slowest stage, and here one stage is
nearly all the work. One thread spends about 200 ns per command, of which matching is about 30 ns (M8: `lob feed`'s apply-only time).
So the output stage (event encoding and FNV hashing, about 62 bytes per command hashed a byte at a time, plus the 70 ns publisher) is about
165 ns. The pipeline's output thread runs at about 260 ns per command (3.8 M/s), so receiving costs it about 90 ns per command.
That's about 2.4 ring items per command (each event, plus `Done`), and each item means the slot's cache line moving from the matching core to the
output core. This is inferred from the numbers, not measured with `perf c2c`.
Fixes, not done: move items in batches (one index store per command, not per event), and split the output stage across threads
(the event hash and the publisher don't depend on each other).

**Latency, paced (scheduled stamps, D48), ring capacity 1024, ns:**

| rate | channel | p50 (3 runs) | p99 | p99.9 | max |
|---|---|---|---|---|---|
| 1 M/s | ring | 823 / 822 / 819 | 15,935 / 14,927 / 17,343 | 124k / 119k / 125k | 437k / 379k / 293k |
| 1 M/s | mpsc | 4,279 / 4,267 / 4,235 | 55,295 / 45,823 / 31,695 | 437k / 268k / 227k | 1.04M / 515k / 495k |
| 2 M/s | ring | 716 / 712 / 712 | 21,423 / 16,735 / 22,111 | 143k / 116k / 145k | 459k / 155k / 508k |
| 3 M/s | ring | 749 / 751 / 769 | 103k / 94k / **2.80M** | 527k / 455k / 3.35M | 591k / 508k / 3.38M |

- **The ring's median is about 5x lower than `mpsc`'s** (0.82 vs 4.3 µs, through two hand-offs and the output stage). `sync_channel` takes a lock
  and can put a waiting thread to sleep, and waking it goes through the kernel. The ring's waiting side spins first.
- **Tails come from the laptop, not the ring.** p99.9 is about 120 µs at 1 M/s with the same machine idle. That size matches scheduler
  preemption and timer interrupts on cores that aren't isolated (`isolcpus`/`nohz_full` were not used). Because stamps are scheduled times, a stall is charged to every command queued behind it.
- **3 M/s is close to the 3.8 M/s capacity.** Run 3 shows what happens near saturation: one stall left a backlog the pipeline drains
  only slowly, and p99 jumped from about 0.1 ms to 2.8 ms. With actual-send stamps, this run would have looked almost as good as the others.
- **Mutation checks:** 11 planted bugs in `ring.rs` and `pipeline.rs`. Weakening `Acquire`/`Release` to `Relaxed` (2 mutants) survives, as D45
  predicts. The final re-check in `pop` survived until a stress test of 20,000 one-item hand-offs was added, which now catches it 3 runs out of 3.

### D49: proptest on stable, not cargo-fuzz (M10)
- **What:** `proptest` (dev-dependency, default features off except `std`) generates inputs from strategies and, on failure,
  *shrinks* them to a minimal counterexample. Failing seeds are saved under `proptest-regressions/` and committed, so a found bug
  is replayed on every later run.
- **Alternatives:** `cargo-fuzz` (libFuzzer) is coverage-guided: it learns which inputs reach new branches, so it finds deep parser
  bugs that random generation misses. It needs nightly Rust, which isn't installed here (the owner decides). `quickcheck` is similar to proptest,
  but its shrinking is per type rather than per strategy, and it's less maintained.
- **Making up for no coverage feedback:** byte inputs aren't only random. Most start from a *valid* encoding and then flip, truncate, insert or
  overwrite bytes, so they get past the first length or tag check and exercise the deeper paths.

### D50: What the properties are
- **Codecs round-trip** for every value of every field: the text command format, the journal record, the event record (fixed sizes), feed messages and
  snapshots, and every decoded ITCH type.
- **Decoders never panic** on any bytes, and say exactly how many bytes they consumed: `decode_command`, `read_journal`, `feed::decode`,
  `decode_snapshot`, `itch::decode` and the ITCH `Reader` over a stream, and `Command::from_str` on any string.
- **The journal never lies:** for any commands, any cut point and any single flipped bit, `read_journal` either fails or returns a
  *prefix* of what was written. If it reports no torn tail, the prefix is everything.
- **The engine:** for any session (narrow prices and small quantities, so trades are common), the reference and fast books emit identical events.
  Both books keep their invariants, and the ledger balances, after every command.
- **The feed:** for any session and any pattern of lost messages, a consumer that gets a snapshot whenever it asks for one ends with the engine's book.

### D51: Case counts
`cargo test` runs proptest's default of 256 cases per property, which takes seconds. A soak run sets `PROPTEST_CASES` (results below). Shrinking is
bounded by proptest's defaults.

### D52: Properties are mutation-checked too
The bugs planted in earlier milestones are planted again. Each must fail a property, and the shrunk counterexample is recorded. That checks that the
properties have teeth, and it shows what shrinking buys over a seeded random test: a few commands instead of thousands.

### M10 results (2026-10-07)
**16 properties** (`tests/codecs.rs` 14, `tests/props.rs` 2). `cargo test` runs 256 cases each. Soak, release build, twice: `PROPTEST_CASES=100000` for the codecs
(1.4M cases, 3.1 s) and `20000` for the engine and feed (40k sessions of up to 150 commands, about 10 s). Both passed. No real bug was found: the M1–M9 decoders were
already strict. In particular, every payload a decoder accepts is the canonical encoding of what it decoded (no trailing bytes, no second spelling).

**Mutation checks (D52):** 16 planted bugs, 11 in the codecs and 5 in the fast book. Codecs: 9 caught at once, 2 survived. The journal's torn-tail condition
(`end == rest.len()` → `end + 1 >= rest.len()`) survived, which led to the D16 property "damage before the last record is an error". It now fails with this shrunk input:
two all-zero limit orders, bit 0 of the first record's CRC flipped, and one byte of the second record left after the cut. ITCH `printable` (`== b'Y'` → `!= b'N'`)
survives. It's equivalent on valid data, since the spec only allows `Y` or `N`.

Shrunk counterexamples for the fast-book bugs (the reference book is correct, so each is the smallest session where the two books differ):

| Planted bug | Shrunk to |
|---|---|
| a new bid beats the best if *lower* | 2 commands: `limit 1 buy 1 98`, `limit 2 buy 1 95 post` |
| ids only need to be ≥ the last (not >) | 2 commands: the same `limit 1 buy 1 95` twice |
| FOK needs *more* than its size available | 10 commands |
| a resting post-only order forgets it's post-only (M2's survivor) | 10 commands, ending in a crossing `modify` of the post-only order |
| a same-quantity, same-price modify loses priority | 22 commands |

**What made shrinking work:** the first session strategy produced 4- to 27-command counterexamples full of `qty 0` limits that do nothing. That had two causes.
Modify and cancel targets were absolute (`id % next`), so removing any earlier command re-pointed them and the failure vanished. And quantities shrink
towards 0, which in this engine turns an order into a rejected no-op instead of removing it. Targets are now "the k-th most recent id", and
alternatives are ordered so values shrink towards 1 (proptest shrinks a union towards its first branch). That cut the two simplest cases to 2 commands.
It also made the generator better at *finding* bugs: the priority bug, which survived 256 cases with the first strategy, is now caught at 256, because relative
targets hit live orders far more often. The filler left in the longer cases comes from proptest's vector shrinker, which tries removing each element only once.
Commands that become removable only after other simplifications stay in.

**Not done then:** coverage-guided fuzzing (`cargo-fuzz` needs nightly; D49; done in M12 as D65), and a stateful model test of the consumer's buffer with reordering (the link never reorders, D42).

### D53: One report, re-measured in one session (M11)
- **What:** `scripts/report.sh` re-runs every throughput and latency measurement from M4–M9, plus a new ITCH-driven workload (D54),
  in one session on one machine. `BENCHMARKS.md` collects the results.
- **Alternatives:** copy the numbers already in this document into one place. They come from six different days and machine states
  (the clock floor, other sessions' load), so putting them side by side would compare things that weren't measured alike.
- **Why also ITCH:** every engine number so far comes from D18's generator. The roadmap promised the reference and fast books on real flow,
  and M7 only replays NASDAQ's book (D37) without running our matching engine.

### D54: ITCH order flow translated into engine commands
- **What:** `lob itch <file> journal <symbol> <out>` turns one symbol's messages into a command journal that both books can replay:

  | ITCH | Command |
  |---|---|
  | `A`/`F` add | `limit` GTC, price in cents (ITCH prices are $0.0001; sub-penny adds are skipped and counted) |
  | `E` execute | `limit` IOC from the other side, at the named order's price, for the executed shares |
  | `X` partial cancel | `modify` down to ITCH's new remaining size (never up, so priority is kept), or `cancel` at 0 |
  | `D` delete | `cancel` |
  | `U` replace | `cancel` the old order, then a new `limit` (NASDAQ gives the replacement new priority) |
  | `C` execute at a cross price | like `X`: we have no auction, so cross executions only remove shares |
  | everything else | nothing |

  Our ids are a fresh increasing counter (D30), mapped from NASDAQ's order references.
- **The engine runs inside the translator.** An `E` names the order NASDAQ filled, but our IOC fills whatever is first in *our* queue at that price.
  Usually that's the same order, but not always (orders we skipped, auction leftovers). The translator applies each command to a reference book
  and tracks every order's size in our book, so it can:
  - skip a cancel or modify for an order our book no longer has, instead of emitting a command that is certain to be rejected
    (executions are always sent: the trade happened, and the IOC keeps the level's volume in step);
  - cancel an order NASDAQ has fully executed but that is still live in our book, so stale liquidity doesn't build up.
  
  The engine is deterministic (D4), so replaying the journal reproduces exactly what the translator saw.
- **Counted:** IOC fills that hit the named order versus another one, IOC shares left unfilled, adds that traded on arrival, resyncs, skipped messages.
  These counts measure how closely our price-time matching agrees with NASDAQ's on displayed orders.
- **Alternatives:** translate only adds and cancels (no matching, so it measures maintaining the book, not matching it), or emit commands blindly
  (simpler, but the books drift apart and the journal fills with rejects that aren't in the real flow).
- **Symbols:** AAPL, and the symbol with the most add orders that day (`lob itch <file> top` lists them).

### D55: Method is D27's
Release build, `scripts/quiet.sh` before each part, a pinned core, warm-up, books alternated, median-p99 run of 5. Only runs whose clock floors are 13–15 ns count.
The report covers throughput (commands/s), latency percentiles by command kind, the feed's cost (M8), end-to-end pipeline latency (ring vs `mpsc`, M9),
the ITCH rebuild rate (M7) and criterion (M6). Every raw output starts with a machine block: CPU, kernel, governor, rustc, and the git commit.

### D56: Raw output is committed, tables are written by hand
`scripts/report.sh` writes each part's raw output to `bench/results/<date>/`, and those files are committed (small text).
`BENCHMARKS.md` is written by hand, and each table names the raw file it comes from.
- **Alternative:** have the script generate the markdown. That's more code to maintain and defend, and the written conclusions still need a person.

### D57: No charts in M11
Tables only. Percentile plots came in M12 (D60).

### M11 results (2026-10-08)
All numbers are in [BENCHMARKS.md](BENCHMARKS.md), with the raw output in `bench/results/2026-10-08-0206/`. The ones that change earlier conclusions:
- **Real flow (D54):** the fast book runs at 28.8 M/s on AAPL and 30.9 M/s on SPY, **2.4x the reference book** (1.55x on generated flow). p99 is 91 / 86 ns, against 230 / 167.
  The gap is wider because of the command mix: real flow is 46–48% cancels and has no rejects.
- **Our matching agrees with NASDAQ:** 98.68% of AAPL's executed shares and 100% of SPY's hit the order NASDAQ named. All 1,881 AAPL fills that disagreed have the same cause:
  NASDAQ filled an order with a lower reference first, even though it was displayed after the order our FIFO filled. So NASDAQ keeps entry-time priority for orders
  displayed late, and our engine, which only sees arrival order, can't reproduce that.
- **Full-depth snapshots don't scale:** recovering AAPL at 1% loss moves 1.14 GB for a 42 MB feed (D42's design assumed shallow books).
- **Method fixes, found while running it:**
  - `cargo bench` compiled its binary *after* the quiet check, so a build that uses every core ran into the measurement. The script now builds everything first.
  - Two latency parts ran with clock floors of 15–21 ns. The script can now redo single parts (`OUT=... ONLY=...`), appending to the same file so both runs stay on record.
- **Translator tests:** 9 tests, including random flow checked against a model of NASDAQ's book. With executions at the queue head, our book equals NASDAQ's after every message.
  With executions anywhere at the touch, every order live in ours is live in NASDAQ's, and both end empty. 13 planted bugs, all caught.

### D58: DESIGN.md keeps its decision log (M12)
- **What:** the D-numbered log stays in the order the decisions were made. An as-built overview and an index of decisions by topic go at the top.
- **Alternatives:** rewrite it by topic, as if designed in one go (loses why things changed, e.g. D7 → D46, D42's snapshots → M11's 1.14 GB);
  leave it as is (a reader can't find "how is cancel O(1)?" without reading 60 entries).
- Outdated wording ("planned", "target after M9") is fixed where it sits, and superseded entries point forward to what replaced them.

### D59: README for a reader with two minutes
Pitch, headline numbers (each linked to [BENCHMARKS.md](BENCHMARKS.md)), architecture, how correctness is checked, try it, code layout, build history.
The milestone-by-milestone feature list and the copy of the benchmark tables go: DESIGN.md and BENCHMARKS.md hold those.

### D60: Percentile plots from full histograms, drawn by our own SVG writer
- **What:** `lob latency <journal> [runs] [dir]` also writes each book's whole-run histogram (the median run's) and the clock floor's to `<dir>/{ref,fast,clock}.hgrm`,
  in HdrHistogram's standard percentile-distribution text format. `lob plot <out.svg> <title> <label=file.hgrm>...` draws them:
  x is `1/(1-percentile)` on a log scale (each decade adds a nine: 90%, 99%, 99.9%...), y is nanoseconds on a log scale.
- **Alternatives:** Python + matplotlib or gnuplot (another toolchain for a reader to install, outside `cargo test`); HdrHistogram's online plotter
  (the `.hgrm` files still load there, but a picture in the repo shouldn't depend on a website); a plotting crate (a big dependency for one chart type).
- **Why log-log:** the interesting part of a latency distribution is the tail, and on a linear percentile axis everything past p99 is squeezed into the last 1%.
  Latencies span 15 ns to hundreds of µs, so a linear y axis would flatten everything below p99.9.
- **Measured in a new run** (the M11 output kept only five percentiles), with D55's method, by `scripts/report.sh` itself.

### D61: What isn't built is written down
A "Not built" section in DESIGN.md (short version in README): what a production exchange has that this engine doesn't, and how each would fit.

### D62: Every performance claim cites a measurement
Every "faster" or number in README and DESIGN.md links to a raw output file or a dated results section (CLAUDE.md's rule). Checked once, at the end of M12.

### D63: Follow-ups stay follow-ups
Top-N snapshots (M11's 1.14 GB recovery), `perf stat` on deep200k's cancel p99, and ITCH with a controlled page cache are listed as future work, not built in M12.

### M12 results (2026-10-08)
- **Percentile plots (D60):** `lob latency ... <dir>` and `lob plot`, 8 tests, 18 planted bugs all caught. The first batch had 5 survivors:
  3 needed tests (the hgrm's 3-column last line, axes rounding out partial decades, a series inside one decade) and 1 was equivalent
  (a special case for p100 that `min` already handled, so the special case was removed). Plots and histograms for all five journals are in
  `bench/results/2026-10-08-0434/`, described in [BENCHMARKS.md](BENCHMARKS.md#latency-percentile-plots-m12-d60).
- **What the plots show that the tables didn't:** past p99.99 the two books meet, and the empty clock window climbs with them
  (0.2–1.3 µs at p99.99, up to 128 µs max). That part of the tail is the machine, not the book; only isolated cores would shrink it.
  The deep queue is the exception: the reference book's cancel scans pull its curve away from the fast book's from about p30 on.

### D64: Miri on the ring (M12)
- **What:** `cargo +nightly miri test --lib ring` runs the ring's 6 tests under Miri, an interpreter that checks every memory access against Rust's rules:
  out-of-bounds or uninitialised reads, use after free, and data races (any two accesses to one location with no happens-before between them).
  It also emulates weak memory: an atomic load can return an older value when the orderings allow it.
- **Only the ring:** it holds the crate's only `unsafe` (the slot write, the slot read, and `Sync` for the shared ring).
- **Smaller loops under Miri:** Miri runs about 1000x slower and chooses thread switches itself, so the threaded tests use 20 repetitions and 500 items
  when `cfg!(miri)` is set (20,000 and 200,000 otherwise). The full suite took over 11 minutes without finishing; the small one takes 5 s.
- **Mutation-checked:** each of the ring's 6 `Acquire`/`Release` operations weakened to `Relaxed`, one at a time: **6 of 6 caught**.
  The 4 on `head` and `tail` show up as data races on a slot (the reader has no happens-before with the writer). The 2 on `closed` fail
  "the last item is never lost", because Miri serves the stale value. On x86 every one of these passes the normal tests (D45).
- **Limit:** Miri samples interleavings; it doesn't try them all. loom would (Future work).

### D65: cargo-fuzz targets for every decoder of outside input
- **What:** `fuzz/` (its own crate; nightly only): `journal` (a command payload, and a whole file), `feed` (a message, and a snapshot), `itch`
  (one message body, and a byte stream through `Reader`), `text` (a command line). None may panic. An accepted command payload, feed message or snapshot
  must be the canonical encoding of what it decodes to (D50). ITCH isn't byte-canonical, so a decoded message must re-encode to one that decodes the same.
  Text must print back to a line that parses to the same command.
- **Why on top of proptest (D49, D50):** proptest generates inputs near valid encodings; libFuzzer learns which inputs reach new branches. They find different bugs.
- **Two lessons from checking the targets:**
  - The first `journal` target fuzzed whole files only. Random bytes almost never carry a valid CRC32, so it never reached the payload decoder (coverage stopped at 184),
    and both planted payload bugs survived. The target now fuzzes the payload decoder directly.
  - `text` never produced `limit ... fok` in 60 s without help. A dictionary of the format's tokens (`fuzz/text.dict`) fixed that.
  - Also: the first `itch` crash in seconds was the target's bug, not the library's. `encode` is documented to panic on message types `decode` skips.
- **Mutation-checked:** 5 planted bugs (an encoder writing FOK as IOC, trailing payload bytes accepted, unknown feed flag bits accepted,
  an ITCH length check loosened, text printing FOK as `ioc`), each fuzzed 60 s: **5 of 5 caught** after the fixes above.

### D66: 10 minutes per target
Each target ran 10 minutes on 2026-10-08 (all with the text dictionary, which only helps `text`). **No crash or failed check in any target:**

| Target | Inputs tried | Rate | Coverage (edges) |
|---|---|---|---|
| journal | 179M | 298k/s | 320 |
| feed | 353M | 588k/s | 192 |
| itch | 9.8M | 16k/s | 333 |
| text | 144M | 240k/s | 275 |

`itch` is slower because each input also goes through the buffered stream `Reader`. Not on a quiet machine: fuzzing finds bugs, it doesn't measure time, so load only changes how many inputs it tries.

### D67: Self-trade prevention by STP group (M13)
- **What:** a new order may carry an STP group, a `u16` the client picks. Two orders in the same group never trade with each other.
  An order without a group never triggers STP and is never cancelled by it.
- **Alternatives:** an account id or a firm id on every order. Exchanges key STP on an id the participant assigns (NASDAQ's
  MPID-level groups, CME's self-match id). The engine doesn't need to know what a group means, only whether two are equal.
- **Why `u16`:** see D72 (it has to fit the fast book's 32-byte order). 65,535 groups is plenty for one book. Group 0 doesn't exist
  (`NonZeroU16`), so "no group" costs nothing in `Option<Stp>`. Existing commands, the generator's default flow and ITCH flow carry no
  group, so their events and the pinned golden digest don't change.

### D68: Three STP actions
- **What:** when an order is about to trade with a resting order of its own group, one of:
  - **cancel newest (`cn`):** cancel the incoming order's remaining quantity. The resting order stays, with its queue spot.
  - **cancel oldest (`co`):** cancel the resting order and keep matching.
  - **cancel both (`cb`):** cancel the resting order and the incoming order's remaining quantity.
- **Not built:** decrement-and-cancel (CME): reduce the larger order by the smaller one's quantity and cancel the smaller. It's a
  partial cancel with its own edge cases (equal quantities, an incoming order that decrements several resting orders) and adds little
  to show. Listed under "Not built".

### D69: The incoming order's action applies
- **What:** the action comes from the incoming (taker) order, the way NASDAQ and CME do it. A resting order's own action doesn't matter
  while someone else trades against it.
- **The resting order still stores its action**, because a modify that re-prices it across the spread makes it the incoming order (D11).

### D70: Where STP happens in matching
- **What:** matching walks the book in price-time order exactly as before (D9). The check is per resting order, just before a fill:
  same group → apply the action instead of trading. **A self-trade never prints.** Fills before that point stand.
- **Event order:** the resting order's cancel first, then the incoming order's (for `cb`).
- **What's left of a cancelled incoming order** is gone for good: it doesn't rest, even if it's a GTC limit.

### D71: FOK and post-only with STP
- **FOK** (D12) must fill completely or do nothing. With STP the pre-check changes:
  - `co`: same-group orders don't count as fillable (they'd be cancelled, not traded). If the order can fill, matching cancels them on the way;
    if it can't, nothing happens, and no resting order is cancelled.
  - `cn` and `cb`: the check stops at the first same-group order, because matching would stop there. So a FOK order with `cn` or `cb`
    either fills completely before reaching its own group or does nothing: it never emits an STP cancel.
- **Fast book cost:** level totals (D21) can't see groups, so a grouped FOK order walks the orders it would hit: O(orders crossed).
  An ungrouped FOK keeps the O(levels) check.
- **Post-only** is unchanged: crossing any resting order, your own group included, is `WouldCross`. A post-only order never trades,
  so it never gets to STP.

### D72: The fast book's order still fits 32 bytes
- **What:** `Node` gains `group: u16` and `stp: u8` (the action; 0 when there's no group): 8 (id) + 8 (qty) + 4 × 3 (level, prev, next)
  + 2 + 1 + 1 (post-only) = 32 bytes. Two orders per cache line still holds (D34), and the compile-time `size_of` assert proves it.
- **Alternative:** a `u32` group would make `Node` 40 bytes (36 rounded up to the 8-byte alignment), so a cache line would hold 1.6 orders.

### D73: A new event and two new journal tags
- **Event:** `SelfTradeCancelled { id, remaining }`, text `stp-cancelled <id> <remaining>`, event tag 6.
  Alternative: a reason field on `Cancelled`, which would change the bytes, and so the digest, of every existing cancel.
- **Text:** optional trailing `g=<group> stp=<cn|co|cb>` on `limit` and `market`, both or neither, so every grouped command has one spelling:
  `limit 7 buy 10 100 ioc g=3 stp=co`.
- **Journal:** new tags 5 (limit + `group u16 | action u8`, 30 bytes) and 6 (market + the same, 21 bytes), version 2.
  Ungrouped commands keep tags 1 and 2 byte for byte, so the decoder reads version 1 files unchanged. One canonical encoding still holds (D50):
  tag 5 or 6 with group 0 is invalid.
  (This refines the consult's "append the fields to limit and market": separate tags cost ungrouped orders nothing and keep v1 readable for free.)

### M13 results (2026-10-08)
- **Correctness:** both books pass the two STP scenario files, written by hand from D70/D71 before either book ran them. Differential testing with
  grouped flows: 40k generated commands × 3 seeds, 10k edge-case × 20 and wide-price × 5 in `cargo test`, and 10M generated + 5M edge-case in the
  release run, all event for event with the ledger's STP checks on. The golden digest is unchanged.
- **Fuzzing:** 5 minutes each on the changed decoders, no crash. `journal`: 77M inputs, coverage 333 edges (320 in M12, so it reaches tags 5 and 6).
  `text` (with `g=` and `stp=` in the dictionary): 54M inputs.
- **Mutation-checked:** 34 planted bugs across the two books, `Stp::conflict`, the text and journal codecs, the ledger and the generator: **34 of 34 caught**.
  One gap was found and closed while writing them: nothing checked that event tags are distinct, so `stp-cancelled` reusing tag 5 would have passed.
- **Cost** ([raw output](bench/results/2026-10-08-m13-stp/)), `lob bench` apply-only on gen2m, no groups, M12 and M13 binaries alternated on a quiet machine:
  fast book 29.2–29.4 M/s before, 27.5–28.3 M/s after, **about 4% slower**. The taker now checks `stp` before every fill and `Node` writes two more fields.
  The reference book didn't move beyond noise (17.6–17.9 vs 18.0–18.5 M/s). With 3 groups (a different workload, mostly more cancels) the fast book
  runs 22.5–25.6 M/s.

### D74: A snapshot is the logical book (M14)
- **What:** `BookState` = the rules (`BookConfig`), the highest id accepted (`last_id`, D30), and every resting order (id, side, price,
  open qty, post-only, STP) in priority order: bids best first, then asks best first, oldest first within a price. Both books implement
  `state()` and `from_state()`, so a snapshot written by either restores into either. The property tests require `fast.state() == ref.state()`
  after every session.
- **Alternative:** dump the fast book's memory (slabs, free lists, ladder windows). Restoring would be a copy, but the format would be tied to
  one book's layout, every M4–M6 change would break old snapshots, and it would carry state (free-list order, window base) that doesn't
  change behaviour.
- **Restore** rebuilds by appending each order to its level in that order, not by replaying `limit` commands: those would fail the id check
  (D30) and could trade. `from_state` validates first: positive quantities within `max_qty`, prices on the grid, unique ids not above `last_id`,
  priority order, an uncrossed book, and a total quantity that fits in `u64` (level totals are `u64`). A book built from a bad state would only
  fail later, far from the cause.

### D75: The snapshot also records where the session stood
- **What:** next to the book, the journal byte offset where the next record starts, and the replay counters (commands, events, trades, rejects)
  with the **running digest**. FNV-1a's state is its output, so hashing resumes from it (`Fnv64::resume`), and event numbers continue too.
- **Why:** with it, recovery is provable: the recovered digest over the whole session equals the uninterrupted run's, not just "the book looks the same".
- `Recorder` (`replay.rs`) is the resumable part of `replay`: counters, digest and event encoding, with the book passed in.

### D76: Snapshot file: whole or not at all
- **Format:** `"LOBS"`, version 1, a fixed header (rules, last id, counters, journal offset, order count), 29-byte order records, and a CRC32 over everything.
  One spelling per value (no group is group 0 with action 0), so accepted bytes are canonical (D50).
- **Write:** to `<path>.tmp`, `fsync`, `rename` over the old snapshot, `fsync` the directory. A crash leaves the old snapshot or the new one.
  Only the latest is kept.
- **Read:** any damage (length, CRC, validation) refuses the file, and recovery replays the journal from the start instead: slower, never wrong.

### D77: Snapshots on the matching thread, every N commands
- **What:** `lob engine ... <every>`, default 100,000 commands. The engine stops matching while it writes, between batches.
- **Alternatives:** copy the book and write from another thread (the pause becomes a copy), or `fork()` and let copy-on-write give a frozen view
  (Redis does this). Both shorten the pause; neither changes what is recovered. The pause is measured (M14 results) and is the number that says whether
  they're worth it.

### D78: Group commit, and no event before its command is durable
- **What:** per batch (default 64 commands): append them all, one `fdatasync`, then apply them and publish their events. With `none` the engine never
  syncs except before a snapshot.
- **The rule:** an event is published only after its command is durable. Otherwise a power cut could lose a command whose trade was already sent,
  and the recovered engine would contradict it.
- **Alternatives:** fsync per command (one disk flush per order; the batch amortizes it) or never (survives a process crash, since the page cache
  outlives the process, but not a power cut). A snapshot always syncs the journal first, in every mode: it must never cover commands the journal can still lose.
- lsmkv's group commit is the same idea.

### D79: Recovery
- Load the snapshot if usable, check its rules match the engine's, replay the journal from its offset, and continue. A restarted `lob engine` does
  exactly this, so restart and recovery are one path.
- **Torn tail** (a crash mid-append): cut off, and the cut made durable, before anything is appended. Otherwise the next record would land after
  garbage, and the file would read as damaged (D16) from then on.
- **Damage before the last record:** refuse (D16). **A snapshot past the end of the journal:** refuse. It describes commands the journal lost.
- **A journal shorter than its header** is a crash during creation: the engine starts a new one.

### D80: One journal, never truncated
- The snapshot only saves time; the journal stays the full history (replay from zero always works). Rotating it into segments and deleting the ones
  a snapshot covers is how it would stay bounded; listed under future work.

### D81: How recovery is proved
- **Crash matrix** (`tests/recovery.rs`): 40 sessions of 200 commands; snapshots at none, the start, the end and 4 random points; the journal cut at every
  record boundary after the snapshot and at one random byte inside every record. For each: recover (both books), run the commands the journal lost,
  and require the uninterrupted run's event bytes, counters, digest and book state.
- **A real crash** (`tests/crash.rs`): the `lob` binary, killed with SIGKILL 7 times at different moments; after each, `lob recover` must report the digest
  of exactly the commands the journal kept. Then a restart runs to the end, and its digest and journal bytes equal an uninterrupted run's.
- **Codecs:** the snapshot property (accepted bytes are canonical, restore into both books, invariants hold), a fuzz target `book_snapshot`
  (it re-seals the CRC so the fuzzer gets past it), and every-cut and every-bit-flip tests.
- **What these can't prove:** that `fsync` is called where it must be. A killed process loses nothing in the page cache; only a power cut (or a
  fault-injecting file system) tells the difference. The sync points are argued in D78 and reviewed, not tested.
