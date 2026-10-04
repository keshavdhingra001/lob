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

## What's built so far

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
- **Command and event model** with a text format whose parser and printer round-trip.

## Planned headline results

- Byte-identical replay of millions of generated orders and a real NASDAQ ITCH session.
- Differential testing of the fast book against a simple reference book.
- p50 / p99 / p99.9 latency per operation, with zero allocations on the hot path.
