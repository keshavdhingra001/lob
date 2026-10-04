//! What goes into the engine (`Command`) and what comes out (`Event`), D4.
//!
//! The engine is a deterministic state machine: the same command sequence always
//! produces the same event sequence. Replay (M3) and differential testing (M4)
//! depend on that.
//!
//! Text format (D6), one command per line, used by the REPL and scenario files:
//!
//! ```text
//! limit  <id> <buy|sell> <qty> <price>
//! market <id> <buy|sell> <qty>
//! cancel <id>
//! ```

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Match against the opposite side up to `price`, then rest the remainder.
    Limit {
        id: OrderId,
        side: Side,
        qty: Qty,
        price: Price,
    },
    /// Match against the opposite side at any price. Never rests.
    Market { id: OrderId, side: Side, qty: Qty },
    /// Remove a resting order.
    Cancel { id: OrderId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The order passed validation. Emitted before any trades it causes.
    Accepted { id: OrderId },
    /// The command was refused and changed nothing.
    Rejected { id: OrderId, reason: RejectReason },
    /// One fill between the incoming order (taker) and one resting order (maker),
    /// at the maker's price.
    Trade {
        taker: OrderId,
        maker: OrderId,
        taker_side: Side,
        qty: Qty,
        price: Price,
    },
    /// The order left the book with `remaining` unfilled: a user cancel, or the
    /// unfillable remainder of a market order.
    Cancelled { id: OrderId, remaining: Qty },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    ZeroQty,
    DuplicateId,
    UnknownOrder,
}

impl Command {
    /// The order id this command refers to.
    pub fn id(&self) -> OrderId {
        match *self {
            Command::Limit { id, .. } | Command::Market { id, .. } | Command::Cancel { id } => id,
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
                expect_args("limit", args, 4)?;
                Ok(Command::Limit {
                    id: OrderId(parse_num("id", args[0])?),
                    side: parse_side(args[1])?,
                    qty: Qty(parse_num("qty", args[2])?),
                    price: Price(parse_num("price", args[3])?),
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
impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Limit {
                id,
                side,
                qty,
                price,
            } => write!(f, "limit {id} {side} {qty} {price}"),
            Command::Market { id, side, qty } => write!(f, "market {id} {side} {qty}"),
            Command::Cancel { id } => write!(f, "cancel {id}"),
        }
    }
}

/// One line per event. Scenario tests compare these lines against expected output.
impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Accepted { id } => write!(f, "accepted {id}"),
            Event::Rejected { id, reason } => write!(f, "rejected {id} {reason}"),
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
            RejectReason::DuplicateId => "duplicate-id",
            RejectReason::UnknownOrder => "unknown-order",
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
        assert_eq!(parse("cancel 1"), Ok(Command::Cancel { id: OrderId(1) }));
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
        for line in ["limit 1 buy 100 10025", "market 2 sell 50", "cancel 3"] {
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
            parse("modify 1"),
            Err(ParseError::UnknownCommand("modify".into()))
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
        ];
        let lines: Vec<String> = events.iter().map(|e| e.to_string()).collect();
        assert_eq!(
            lines,
            [
                "accepted 2",
                "trade 2 1 sell 40 10025",
                "cancelled 2 10",
                "rejected 9 unknown-order",
            ]
        );
    }
}
