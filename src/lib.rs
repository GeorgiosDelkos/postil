//! Postil: read a repository, write review notes on lines, send them to a coding agent.
//!
//! The design and its reasons are in `docs/design.md`. In short: this Rust process owns the
//! data and a page in the system webview draws it. The page is compiled into the binary and
//! talks to this process over a private URL scheme.
//!
//! Built so far (milestones 0 and 1): the window, the file list with its fuzzy finder, and
//! the highlighted code view.

mod error;
mod highlight;
mod repo;
mod shell;

pub use crate::error::Error;
pub use crate::shell::{BenchReport, bench, run};
