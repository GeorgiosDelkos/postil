//! The Agent Client Protocol client: starts a coding agent, opens a session in the
//! repository, sends it prompts, and passes on what it reports.
//!
//! ACP is JSON-RPC 2.0, one JSON object per line, over the agent's standard input and output
//! (docs/design.md, "ACP integration"). The app is the client and speaks version 1. It
//! advertises no file or terminal capability, so the only request an agent sends that the
//! app serves is a permission request.
//!
//! # Threads
//!
//! An [`Agent`] has three threads, all ended by its process exiting:
//!
//! - the writer sends the queued lines to standard input, so no caller blocks on an agent
//!   that stopped reading;
//! - the reader reads standard output, hands each reply to whoever waits for it, and turns
//!   everything else into an [`Event`] for the callback given to [`Agent::start`];
//! - the stderr thread keeps the end of standard error for error messages, and keeps the
//!   agent from blocking on a full pipe.
//!
//! The caller's thread and the reader share one lock, over the requests in flight in both
//! directions. Nothing is called while it is held: the event callback runs after it is
//! released.
//!
//! Dropping the [`Agent`] marks it closed and kills its process group; the reader then
//! delivers no more events, and the threads end as the pipes close.

mod connection;
mod wire;

pub(crate) use crate::acp::connection::{Agent, Event, Launch, RequestId, TurnEnd};
pub(crate) use crate::acp::wire::{
    ContentBlock, Location, PermissionKind, PlanEntryStatus, SessionUpdate, StopReason,
    ToolCallContent, ToolCallStatus, ToolKind,
};
