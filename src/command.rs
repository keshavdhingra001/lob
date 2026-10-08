//! What goes into the engine (`Command`) and what comes out (`Event`), D4.
//!
//! The engine is a deterministic state machine: the same command sequence always
//! produces the same event sequence. Replay (M3) and differential testing (M4)
//! depend on that.
//!
//! Text format (D6), one command per line, used by the REPL and scenario files:
//!
//! ```text
//! limit  <id> <buy|sell> <qty> <price> [gtc|ioc|fok|post] [peak=<n>] [g=<group> stp=<cn|co|cb>]
//! market <id> <buy|sell> <qty> [g=<group> stp=<cn|co|cb>]
//! modify <id> <qty> <price>
//! cancel <id>
//! ```

use std::fmt;
use std::num::NonZeroU16;
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
        /// An iceberg (D83): only `peak` of the resting quantity shows at a time. The book
        /// checks `0 < peak < qty` and a resting time in force.
        peak: Option<Qty>,
        stp: Option<Stp>,
    },
    /// Match against the opposite side at any price. Never rests.
    Market {
        id: OrderId,
        side: Side,
        qty: Qty,
        stp: Option<Stp>,
    },
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

/// Self-trade prevention (D67): two orders with the same group never trade with each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stp {
    pub group: NonZeroU16,
    /// What happens instead of a self-trade. The incoming order's action applies (D69).
    pub action: StpAction,
}

impl Stp {
    /// The action to take instead of a trade between an incoming order (`taker`) and a
    /// resting one (`maker`): the taker's action if both are in the same group (D69, D70).
    pub fn conflict(taker: Option<Stp>, maker: Option<Stp>) -> Option<StpAction> {
        match (taker, maker) {
            (Some(t), Some(m)) if t.group == m.group => Some(t.action),
            _ => None,
        }
    }
}

/// D68.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StpAction {
    /// Cancel the incoming order's remaining quantity; the resting order stays.
    CancelNewest,
    /// Cancel the resting order and keep matching.
    CancelOldest,
    /// Cancel both.
    CancelBoth,
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
    /// The order is done with `remaining` unfilled, cancelled by self-trade prevention
    /// instead of trading with an order of its own group (D70, D73).
    SelfTradeCancelled { id: OrderId, remaining: Qty },
    /// A resting iceberg's shown quantity ran out and `qty` more of its hidden quantity now
    /// shows, at the back of its level (D84, D88).
    Replenished { id: OrderId, qty: Qty },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    ZeroQty,
    QtyTooLarge,
    BadTick,
    /// A new order's id must be greater than every id accepted before it this session
    /// (D30). Reusing an id is the common case of this.
    IdNotIncreasing,
    UnknownOrder,
    /// A post-only order (or a modify of one) would have traded.
    WouldCross,
    /// An iceberg's peak must be positive, below its quantity, and on an order that can
    /// rest (GTC or post-only) (D83).
    BadPeak,
}

/// A limit or market order as both books match it (`limit` is `None` for a market order,
/// whose time in force is GTC: with no limit it can't rest anyway).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewOrder {
    pub id: OrderId,
    pub side: Side,
    pub qty: Qty,
    pub limit: Option<Price>,
    pub tif: TimeInForce,
    pub peak: Option<Qty>,
    pub stp: Option<Stp>,
}

impl Command {
    /// The new order this command places, if it places one.
    pub fn new_order(&self) -> Option<NewOrder> {
        match *self {
            Command::Limit {
                id,
                side,
                qty,
                price,
                tif,
                peak,
                stp,
            } => Some(NewOrder {
                id,
                side,
                qty,
                limit: Some(price),
                tif,
                peak,
                stp,
            }),
            Command::Market { id, side, qty, stp } => Some(NewOrder {
                id,
                side,
                qty,
                limit: None,
                tif: TimeInForce::Gtc,
                peak: None,
                stp,
            }),
            Command::Modify { .. } | Command::Cancel { .. } => None,
        }
    }

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
                let (args, peak, stp) = split_named(args)?;
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
                    peak,
                    stp,
                })
            }
            "market" => {
                let (args, peak, stp) = split_named(args)?;
                if peak.is_some() {
                    return Err(ParseError::PeakNotAllowed);
                }
                expect_args("market", args, 3)?;
                Ok(Command::Market {
                    id: OrderId(parse_num("id", args[0])?),
                    side: parse_side(args[1])?,
                    qty: Qty(parse_num("qty", args[2])?),
                    stp,
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

impl FromStr for StpAction {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        match s {
            "cn" => Ok(StpAction::CancelNewest),
            "co" => Ok(StpAction::CancelOldest),
            "cb" => Ok(StpAction::CancelBoth),
            _ => Err(ParseError::BadStp(s.to_string())),
        }
    }
}

/// Split the optional named arguments off a new order's positional ones. They come in
/// this order: `peak=<n>` (D83), then `g=<group> stp=<action>` together (D73), so each
/// command has exactly one spelling.
/// A new order's positional arguments, its peak and its STP pair.
type Split<'a> = (&'a [&'a str], Option<Qty>, Option<Stp>);

fn split_named<'a>(args: &'a [&'a str]) -> Result<Split<'a>, ParseError> {
    let at = args
        .iter()
        .position(|a| a.contains('='))
        .unwrap_or(args.len());
    let (positional, mut named) = args.split_at(at);
    let mut peak = None;
    if let Some(value) = named.first().and_then(|a| a.strip_prefix("peak=")) {
        // A zero or oversized peak parses: whether it's valid depends on the order (D83).
        peak = Some(Qty(parse_num("peak", value)?));
        named = &named[1..];
    }
    Ok((positional, peak, parse_stp(named)?))
}

/// `g=<group> stp=<action>`, or nothing.
fn parse_stp(named: &[&str]) -> Result<Option<Stp>, ParseError> {
    if named.is_empty() {
        return Ok(None);
    }
    let bad = || ParseError::BadStp(named.join(" "));
    let [group, action] = named else {
        return Err(bad());
    };
    let group = group.strip_prefix("g=").ok_or_else(bad)?;
    let action = action.strip_prefix("stp=").ok_or_else(bad)?;
    Ok(Some(Stp {
        // `NonZeroU16` refuses 0: "no group" is spelled by leaving the pair out.
        group: parse_num("group", group)?,
        action: action.parse()?,
    }))
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
                peak,
                stp,
            } => {
                write!(f, "limit {id} {side} {qty} {price}")?;
                if *tif != TimeInForce::Gtc {
                    write!(f, " {tif}")?;
                }
                if let Some(peak) = peak {
                    write!(f, " peak={peak}")?;
                }
                write_stp(f, stp)
            }
            Command::Market { id, side, qty, stp } => {
                write!(f, "market {id} {side} {qty}")?;
                write_stp(f, stp)
            }
            Command::Modify { id, qty, price } => write!(f, "modify {id} {qty} {price}"),
            Command::Cancel { id } => write!(f, "cancel {id}"),
        }
    }
}

fn write_stp(f: &mut fmt::Formatter<'_>, stp: &Option<Stp>) -> fmt::Result {
    match stp {
        Some(Stp { group, action }) => write!(f, " g={group} stp={action}"),
        None => Ok(()),
    }
}

impl fmt::Display for StpAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StpAction::CancelNewest => "cn",
            StpAction::CancelOldest => "co",
            StpAction::CancelBoth => "cb",
        })
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
            Event::SelfTradeCancelled { id, remaining } => {
                write!(f, "stp-cancelled {id} {remaining}")
            }
            Event::Replenished { id, qty } => write!(f, "replenished {id} {qty}"),
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RejectReason::ZeroQty => "zero-qty",
            RejectReason::QtyTooLarge => "qty-too-large",
            RejectReason::BadTick => "bad-tick",
            RejectReason::IdNotIncreasing => "id-not-increasing",
            RejectReason::UnknownOrder => "unknown-order",
            RejectReason::WouldCross => "would-cross",
            RejectReason::BadPeak => "bad-peak",
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
                peak: None,
                stp: None,
            })
        );
        assert_eq!(
            parse("market 2 sell 50"),
            Ok(Command::Market {
                id: OrderId(2),
                side: Side::Sell,
                qty: Qty(50),
                stp: None,
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
            "limit 5 buy 1 100 g=7 stp=cn",
            "limit 5 buy 1 100 ioc g=65535 stp=co",
            "market 6 sell 2 g=1 stp=cb",
            "limit 7 buy 100 50 peak=10",
            "limit 8 sell 100 50 post peak=1 g=2 stp=co",
            "limit 9 sell 1 50 ioc peak=0",
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
    fn stp_is_a_trailing_pair() {
        let stp = |line: &str| match parse(line) {
            Ok(Command::Limit { stp, .. } | Command::Market { stp, .. }) => Ok(stp),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        assert_eq!(stp("limit 1 buy 1 100"), Ok(None));
        assert_eq!(
            stp("market 1 buy 1 g=3 stp=co"),
            Ok(Some(Stp {
                group: NonZeroU16::new(3).unwrap(),
                action: StpAction::CancelOldest,
            }))
        );
        let bad = |s: &str| Err(ParseError::BadStp(s.into()));
        // Both or neither, group first, and nothing after.
        assert_eq!(stp("limit 1 buy 1 100 g=3"), bad("g=3"));
        assert_eq!(stp("limit 1 buy 1 100 stp=cn"), bad("stp=cn"));
        assert_eq!(stp("limit 1 buy 1 100 stp=cn g=3"), bad("stp=cn g=3"));
        assert_eq!(
            stp("limit 1 buy 1 100 g=3 stp=cn ioc"),
            bad("g=3 stp=cn ioc")
        );
        assert_eq!(stp("limit 1 buy 1 100 g=3 stp=xx"), bad("xx"));
        // Group 0 doesn't exist: no group is spelled by leaving the pair out.
        for group in ["0", "65536", "-1"] {
            assert_eq!(
                stp(&format!("limit 1 buy 1 100 g={group} stp=cn")),
                Err(ParseError::BadNumber {
                    field: "group",
                    value: group.into(),
                })
            );
        }
        // Only new orders carry a group.
        assert!(parse("cancel 1 g=3 stp=cn").is_err());
        assert!(parse("modify 1 1 100 g=3 stp=cn").is_err());
    }

    #[test]
    fn peak_comes_before_the_stp_pair() {
        let named = |line: &str| match parse(line) {
            Ok(Command::Limit { peak, stp, .. }) => Ok((peak, stp.map(|s| s.group.get()))),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        assert_eq!(named("limit 1 buy 9 100 peak=3"), Ok((Some(Qty(3)), None)));
        assert_eq!(
            named("limit 1 buy 9 100 post peak=3 g=4 stp=cn"),
            Ok((Some(Qty(3)), Some(4)))
        );
        // The book decides whether a peak is valid for the order (D83), so 0 parses.
        assert_eq!(named("limit 1 buy 9 100 peak=0"), Ok((Some(Qty(0)), None)));
        assert_eq!(
            named("limit 1 buy 9 100 g=4 stp=cn peak=3"),
            Err(ParseError::BadStp("g=4 stp=cn peak=3".into()))
        );
        assert_eq!(
            named("limit 1 buy 9 100 peak=3 peak=3"),
            Err(ParseError::BadStp("peak=3".into()))
        );
        assert_eq!(
            named("limit 1 buy 9 100 peak=-1"),
            Err(ParseError::BadNumber {
                field: "peak",
                value: "-1".into(),
            })
        );
        assert_eq!(
            named("limit 1 buy 9 100 peak=3 ioc"),
            Err(ParseError::BadStp("ioc".into()))
        );
        assert_eq!(
            parse("market 1 buy 9 peak=3"),
            Err(ParseError::PeakNotAllowed)
        );
        assert!(parse("modify 1 9 100 peak=3").is_err());
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
            Event::SelfTradeCancelled {
                id: OrderId(4),
                remaining: Qty(6),
            },
            Event::Replenished {
                id: OrderId(5),
                qty: Qty(10),
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
                "stp-cancelled 4 6",
                "replenished 5 10",
            ]
        );
    }
}
