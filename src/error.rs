use thiserror::Error;

/// A line of the text command format (D6) that couldn't be parsed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty line")]
    Empty,
    #[error("unknown command `{0}` (expected limit, market or cancel)")]
    UnknownCommand(String),
    #[error("`{command}` takes {expected} arguments, got {got}")]
    WrongArgCount {
        command: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("invalid side `{0}` (expected buy or sell)")]
    BadSide(String),
    #[error("invalid {field} `{value}`")]
    BadNumber { field: &'static str, value: String },
}
