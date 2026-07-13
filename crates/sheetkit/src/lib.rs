//! sheetkit — an LLM-native spreadsheet toolkit built on the IronCalc engine.
//!
//! Spreadsheet interfaces built for humans (grids) or machines (verbose cell
//! APIs) both fail language models. sheetkit rebuilds the three things that
//! make a spreadsheet usable — random-access vision, instant recalc feedback,
//! and spatial addressing — as *text*:
//!
//! - compressed, structure-aware **views** ([`view`]) under explicit token budgets
//! - a **delta echo** after every mutation ([`delta`]): what changed, old ⇒ new
//! - **range-level verbs** ([`cmd`]): fill, sort, find, name — not cell-by-cell calls
//!
//! The [`session`] module ties it together for the `sheetd` server, which
//! exposes everything as MCP tools and a REPL.

pub mod addr;
pub mod book;
pub mod cmd;
pub mod delta;
pub mod displace;
pub mod fills;
pub mod regions;
pub mod session;
pub mod view;

use std::fmt;

/// The crate-wide error type: a message meant to be shown to the caller
/// (which is usually a language model — messages should say what to do next).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
