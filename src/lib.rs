//! lob: a price-time priority limit order book and matching engine.
//!
//! See DESIGN.md for decisions and CHECKPOINT.md for the roadmap.

pub mod book;
pub mod command;
pub mod error;
pub mod types;

pub use book::{Level, OrderBook};
pub use command::{Command, Event, RejectReason};
pub use error::ParseError;
pub use types::{OrderId, Price, Qty, Side};
