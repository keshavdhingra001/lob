# lob design

Living document. Every non-obvious decision gets a short entry: **what**, **alternatives**, **why**.

## Architecture (target, after M9)

```
clients ──> gateway thread ──SPSC ring──> matching thread ──SPSC ring──> market data / journal
                 │                         (one per symbol)                    │
                 └── decode + validate     Command in, Events out          L2 updates, trades
                                           pure, deterministic             command journal (replay)
```

Today (M3): the reference book (D8) behind the `OrderBook` trait, matching by D9 with modify
(D11), IOC/FOK/post-only (D12) and instrument rules (D14). Checked by scenario scripts, an
invariant checker (D10) and an event-only conservation ledger over random sessions (D13).
Input is recorded in a binary journal (D15, D16); replay produces a sequenced event stream and a
digest (D17). A seeded generator (D18) supplies realistic order flow.

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
