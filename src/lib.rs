//! lob: a price-time priority limit order book and matching engine.
//!
//! See DESIGN.md for decisions and CHECKPOINT.md for the roadmap.

pub mod book;
pub mod command;
pub mod error;
pub mod ledger;
pub mod reference;
pub mod rng;
pub mod scenario;
pub mod types;

pub use book::{BookConfig, Level, OrderBook};
pub use command::{Command, Event, RejectReason, TimeInForce};
pub use error::ParseError;
pub use reference::RefBook;
pub use types::{OrderId, Price, Qty, Side};
