//! What goes into the engine (`Command`) and what comes out (`Event`), D4.
//!
//! The engine is a deterministic state machine: the same command sequence always
//! produces the same event sequence. Replay (M3) and differential testing (M4)
//! depend on that.
//!
//! Text format (D6), one command per line, used by the REPL and scenario files:
//!
//! ```text
//! limit  <id> <buy|sell> <qty> <price> [gtc|ioc|fok|post]
//! market <id> <buy|sell> <qty>
//! modify <id> <qty> <price>
//! cancel <id>
//! ```

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Match against the opposite side up to `price`; `tif` decides what happens to the rest.
    Limit {
        id: OrderId,
        side: Side,
        qty: Qty,
        price: Price,
        tif: TimeInForce,
    },
    /// Match against the opposite side at any price. Never rests.
    Market { id: OrderId, side: Side, qty: Qty },
    /// Change a resting order's open quantity and/or price (D11).
    Modify { id: OrderId, qty: Qty, price: Price },
    /// Remove a resting order.
    Cancel { id: OrderId },
}

/// What a limit order does with quantity it can't fill immediately (D12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TimeInForce {
    /// Good till cancelled: rest the remainder on the book.
    #[default]
    Gtc,
    /// Immediate or cancel: fill what's possible now, cancel the rest.
    Ioc,
    /// Fill or kill: fill all of it now, or none of it.
    Fok,
    /// Only ever add liquidity: rejected if it would trade on arrival.
    PostOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The order passed validation. Emitted before any trades it causes.
    Accepted { id: OrderId },
    /// The command was refused and changed nothing.
    Rejected { id: OrderId, reason: RejectReason },
    /// A resting order now has open quantity `qty` at `price`. Emitted before any
    /// trades the new price causes.
    Modified { id: OrderId, qty: Qty, price: Price },
    /// One fill between the incoming order (taker) and one resting order (maker),
    /// at the maker's price.
    Trade {
        taker: OrderId,
        maker: OrderId,
        taker_side: Side,
        qty: Qty,
        price: Price,
    },
    /// The order is done with `remaining` unfilled: a user cancel, the unfillable rest
    /// of a market or IOC order, or a FOK order that couldn't fill completely.
    Cancelled { id: OrderId, remaining: Qty },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    ZeroQty,
    QtyTooLarge,
    BadTick,
    DuplicateId,
    UnknownOrder,
    /// A post-only order (or a modify of one) would have traded.
    WouldCross,
}

impl Command {
    /// The order id this command refers to.
    pub fn id(&self) -> OrderId {
        match *self {
            Command::Limit { id, .. }
            | Command::Market { id, .. }
            | Command::Modify { id, .. }
            | Command::Cancel { id } => id,
        }
    }
}

impl FromStr for Command {
    type Err = ParseError;

    fn from_str(line: &str) -> Result<Self, ParseError> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (&name, args) = tokens.split_first().ok_or(ParseError::Empty)?;
        match name {
            "limit" => {
                // The time in force is optional, so `limit` takes 4 or 5 arguments.
                if args.len() != 5 {
                    expect_args("limit", args, 4)?;
                }
                Ok(Command::Limit {
                    id: OrderId(parse_num("id", args[0])?),
                    side: parse_side(args[1])?,
                    qty: Qty(parse_num("qty", args[2])?),
                    price: Price(parse_num("price", args[3])?),
                    tif: args.get(4).map_or(Ok(TimeInForce::Gtc), |s| s.parse())?,
                })
            }
            "market" => {
                expect_args("market", args, 3)?;
                Ok(Command::Market {
                    id: OrderId(parse_num("id", args[0])?),
                    side: parse_side(args[1])?,
                    qty: Qty(parse_num("qty", args[2])?),
                })
            }
            "modify" => {
                expect_args("modify", args, 3)?;
                Ok(Command::Modify {
                    id: OrderId(parse_num("id", args[0])?),
                    qty: Qty(parse_num("qty", args[1])?),
                    price: Price(parse_num("price", args[2])?),
                })
            }
            "cancel" => {
                expect_args("cancel", args, 1)?;
                Ok(Command::Cancel {
                    id: OrderId(parse_num("id", args[0])?),
                })
            }
            other => Err(ParseError::UnknownCommand(other.to_string())),
        }
    }
}

impl FromStr for TimeInForce {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        match s {
            "gtc" => Ok(TimeInForce::Gtc),
            "ioc" => Ok(TimeInForce::Ioc),
            "fok" => Ok(TimeInForce::Fok),
            "post" => Ok(TimeInForce::PostOnly),
            _ => Err(ParseError::BadTimeInForce(s.to_string())),
        }
    }
}

fn expect_args(command: &'static str, args: &[&str], expected: usize) -> Result<(), ParseError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(ParseError::WrongArgCount {
            command,
            expected,
            got: args.len(),
        })
    }
}

fn parse_side(s: &str) -> Result<Side, ParseError> {
    match s {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        _ => Err(ParseError::BadSide(s.to_string())),
    }
}

fn parse_num<T: FromStr>(field: &'static str, s: &str) -> Result<T, ParseError> {
    s.parse().map_err(|_| ParseError::BadNumber {
        field,
        value: s.to_string(),
    })
}

/// Prints the same format `from_str` parses, so `cmd.to_string().parse() == Ok(cmd)`.
/// GTC is the default, so it isn't printed.
impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Limit {
                id,
                side,
                qty,
                price,
                tif,
            } => {
                write!(f, "limit {id} {side} {qty} {price}")?;
                match tif {
                    TimeInForce::Gtc => Ok(()),
                    tif => write!(f, " {tif}"),
                }
            }
            Command::Market { id, side, qty } => write!(f, "market {id} {side} {qty}"),
            Command::Modify { id, qty, price } => write!(f, "modify {id} {qty} {price}"),
            Command::Cancel { id } => write!(f, "cancel {id}"),
        }
    }
}

impl fmt::Display for TimeInForce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimeInForce::Gtc => "gtc",
            TimeInForce::Ioc => "ioc",
            TimeInForce::Fok => "fok",
            TimeInForce::PostOnly => "post",
        })
    }
}

/// One line per event. Scenario tests compare these lines against expected output.
impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Accepted { id } => write!(f, "accepted {id}"),
            Event::Rejected { id, reason } => write!(f, "rejected {id} {reason}"),
            Event::Modified { id, qty, price } => write!(f, "modified {id} {qty} {price}"),
            Event::Trade {
                taker,
                maker,
                taker_side,
                qty,
                price,
            } => write!(f, "trade {taker} {maker} {taker_side} {qty} {price}"),
            Event::Cancelled { id, remaining } => write!(f, "cancelled {id} {remaining}"),
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RejectReason::ZeroQty => "zero-qty",
            RejectReason::QtyTooLarge => "qty-too-large",
            RejectReason::BadTick => "bad-tick",
            RejectReason::DuplicateId => "duplicate-id",
            RejectReason::UnknownOrder => "unknown-order",
            RejectReason::WouldCross => "would-cross",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Command, ParseError> {
        s.parse()
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(
            parse("limit 1 buy 100 10025"),
            Ok(Command::Limit {
                id: OrderId(1),
                side: Side::Buy,
                qty: Qty(100),
                price: Price(10025),
                tif: TimeInForce::Gtc,
            })
        );
        assert_eq!(
            parse("market 2 sell 50"),
            Ok(Command::Market {
                id: OrderId(2),
                side: Side::Sell,
                qty: Qty(50),
            })
        );
        assert_eq!(
            parse("modify 3 40 10010"),
            Ok(Command::Modify {
                id: OrderId(3),
                qty: Qty(40),
                price: Price(10010),
            })
        );
        assert_eq!(parse("cancel 1"), Ok(Command::Cancel { id: OrderId(1) }));
    }

    #[test]
    fn time_in_force_is_optional_and_defaults_to_gtc() {
        let tif = |line: &str| match parse(line) {
            Ok(Command::Limit { tif, .. }) => tif,
            other => panic!("{other:?}"),
        };
        assert_eq!(tif("limit 1 buy 1 100"), TimeInForce::Gtc);
        assert_eq!(tif("limit 1 buy 1 100 gtc"), TimeInForce::Gtc);
        assert_eq!(tif("limit 1 buy 1 100 ioc"), TimeInForce::Ioc);
        assert_eq!(tif("limit 1 buy 1 100 fok"), TimeInForce::Fok);
        assert_eq!(tif("limit 1 buy 1 100 post"), TimeInForce::PostOnly);
        assert_eq!(
            parse("limit 1 buy 1 100 day"),
            Err(ParseError::BadTimeInForce("day".into()))
        );
        assert_eq!(
            parse("limit 1 buy 1 100 ioc x"),
            Err(ParseError::WrongArgCount {
                command: "limit",
                expected: 4,
                got: 6,
            })
        );
    }

    #[test]
    fn tolerates_extra_whitespace() {
        assert_eq!(
            parse("  cancel \t 7  "),
            Ok(Command::Cancel { id: OrderId(7) })
        );
    }

    #[test]
    fn negative_price_is_allowed() {
        assert!(matches!(
            parse("limit 1 sell 5 -20"),
            Ok(Command::Limit {
                price: Price(-20),
                ..
            })
        ));
    }

    #[test]
    fn display_round_trips() {
        for line in [
            "limit 1 buy 100 10025",
            "limit 1 buy 100 10025 ioc",
            "limit 1 sell 100 -5 fok",
            "limit 1 sell 100 10025 post",
            "market 2 sell 50",
            "modify 4 10 10030",
            "cancel 3",
        ] {
            let cmd = parse(line).unwrap();
            assert_eq!(cmd.to_string(), line);
            assert_eq!(parse(&cmd.to_string()), Ok(cmd));
        }
    }

    #[test]
    fn rejects_malformed_lines() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert_eq!(
            parse("replace 1"),
            Err(ParseError::UnknownCommand("replace".into()))
        );
        assert_eq!(
            parse("limit 1 buy 100"),
            Err(ParseError::WrongArgCount {
                command: "limit",
                expected: 4,
                got: 3,
            })
        );
        assert_eq!(
            parse("cancel 1 2"),
            Err(ParseError::WrongArgCount {
                command: "cancel",
                expected: 1,
                got: 2,
            })
        );
        assert_eq!(
            parse("market 1 hold 5"),
            Err(ParseError::BadSide("hold".into()))
        );
    }

    #[test]
    fn rejects_bad_numbers() {
        // Quantities and ids are unsigned, prices are integer ticks: no floats.
        assert_eq!(
            parse("market 1 buy -5"),
            Err(ParseError::BadNumber {
                field: "qty",
                value: "-5".into(),
            })
        );
        assert_eq!(
            parse("limit 1 buy 5 100.25"),
            Err(ParseError::BadNumber {
                field: "price",
                value: "100.25".into(),
            })
        );
        assert_eq!(
            parse("cancel x"),
            Err(ParseError::BadNumber {
                field: "id",
                value: "x".into(),
            })
        );
    }

    #[test]
    fn events_print_one_line_each() {
        let events = [
            Event::Accepted { id: OrderId(2) },
            Event::Trade {
                taker: OrderId(2),
                maker: OrderId(1),
                taker_side: Side::Sell,
                qty: Qty(40),
                price: Price(10025),
            },
            Event::Cancelled {
                id: OrderId(2),
                remaining: Qty(10),
            },
            Event::Rejected {
                id: OrderId(9),
                reason: RejectReason::UnknownOrder,
            },
            Event::Modified {
                id: OrderId(3),
                qty: Qty(5),
                price: Price(99),
            },
        ];
        let lines: Vec<String> = events.iter().map(|e| e.to_string()).collect();
        assert_eq!(
            lines,
            [
                "accepted 2",
                "trade 2 1 sell 40 10025",
                "cancelled 2 10",
                "rejected 9 unknown-order",
                "modified 3 5 99",
            ]
        );
    }
}
