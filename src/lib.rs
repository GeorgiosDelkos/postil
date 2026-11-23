//! Postil: read a repository, write review notes on lines, send them to a coding agent.
//!
//! The design and its reasons are in `docs/design.md`. In short: this Rust process owns the
//! data and a page in the system webview draws it. The page is compiled into the binary and
//! talks to this process over a private URL scheme.
//!
//! Built so far (milestones 0 to 2, and the core of 3): the window, the file list with its
//! fuzzy finder, the highlighted code view, notes on line ranges, stored in the git directory
//! and anchored by their text, and runs that send notes to a coding agent over ACP.

mod acp;
mod agents;
mod dialog;
mod error;
mod files;
mod highlight;
mod layout;
#[cfg(target_os = "macos")]
mod menu;
mod notes;
mod recent;
mod repo;
mod run;
mod settings;
mod shell;

pub use crate::error::Error;
pub use crate::shell::{BenchReport, bench, run};
