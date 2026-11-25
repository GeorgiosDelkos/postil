//! Postil: read a repository, write review notes on lines, send them to a coding agent.
//!
//! The design and its reasons are in `docs/design.md`. In short: this Rust process owns the
//! data and a page in the system webview draws it. The page is compiled into the binary and
//! talks to this process over a private URL scheme. What is built so far is tracked in the
//! design's "Milestones" section.

mod acp;
mod agents;
mod clone;
mod dialog;
mod disk;
mod error;
mod files;
mod folders;
mod highlight;
mod layout;
#[cfg(target_os = "macos")]
mod menu;
mod notes;
mod recent;
mod repo;
mod review;
mod run;
mod settings;
mod shell;
mod site;
#[cfg(test)]
mod testing;
mod thread;

pub use crate::error::{Error, error_chain};
pub use crate::shell::{BenchReport, bench, run};
