use thiserror::Error;

/// A line of the text command format (D6) that couldn't be parsed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty line")]
    Empty,
    #[error("unknown command `{0}` (expected limit, market, modify or cancel)")]
    UnknownCommand(String),
    #[error("`{command}` takes {expected} arguments, got {got}")]
    WrongArgCount {
        command: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("invalid side `{0}` (expected buy or sell)")]
    BadSide(String),
    #[error("invalid time in force `{0}` (expected gtc, ioc, fok or post)")]
    BadTimeInForce(String),
    #[error("invalid self-trade prevention `{0}` (expected g=<1-65535> stp=<cn|co|cb>)")]
    BadStp(String),
    #[error("invalid {field} `{value}`")]
    BadNumber { field: &'static str, value: String },
}
