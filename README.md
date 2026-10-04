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
> limit 1 buy 100 10025
parsed: limit 1 buy 100 10025
> market 2 sell 40
parsed: market 2 sell 40
```

Prices are integer ticks (`10025` is $100.25 with a one-cent tick). Matching arrives in M1.

## What's built so far

- **Command and event model**: limit, market and cancel in; accepted, rejected, trade and cancelled out.
- **Text format** with a parser and printer that round-trip, used by the REPL and scenario tests.
- **`OrderBook` trait** that the reference book (M1) and the fast book (M4) both implement.

## Planned headline results

- Byte-identical replay of millions of generated orders and a real NASDAQ ITCH session.
- Differential testing of the fast book against a simple reference book.
- p50 / p99 / p99.9 latency per operation, with zero allocations on the hot path.
