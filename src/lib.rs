//! lob: a price-time priority limit order book and matching engine.
//!
//! See DESIGN.md for decisions and README.md for the roadmap.

pub mod book;
pub mod command;
pub mod consumer;
pub mod error;
pub mod fast;
pub mod feed;
pub mod gen;
pub mod hash;
pub mod itch;
pub mod itch_book;
pub mod itch_flow;
pub mod journal;
pub mod ladder;
pub mod latency;
pub mod ledger;
pub mod pipeline;
pub mod plot;
pub mod reference;
pub mod replay;
pub mod ring;
pub mod rng;
pub mod scenario;
pub mod types;

pub use book::{BookConfig, Level, OrderBook};
pub use command::{Command, Event, RejectReason, Stp, StpAction, TimeInForce};
pub use error::ParseError;
pub use fast::FastBook;
pub use reference::RefBook;
pub use types::{OrderId, Price, Qty, Side};
