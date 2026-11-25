//! Usage statistics for autodetected and custom coding agents.
//!
//! Combines two complementary sources:
//! - [`ledger`]: Postil's durable append-only ledger (`<config dir>/usage.jsonl`), recording
//!   every completed run across projects with its agent, model, token counts, cost, and
//!   duration so deleting notes never erases history;
//! - [`quota`]: on-demand background probes for live provider account quotas and rate-limit
//!   windows (Claude, Codex, Gemini, and graceful fallback for others), cached briefly in
//!   memory so an idle window does zero work.

pub(crate) mod ledger;
pub(crate) mod quota;

use std::time::SystemTime;

/// Seconds since the Unix epoch, or `0` if the system clock precedes it.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
