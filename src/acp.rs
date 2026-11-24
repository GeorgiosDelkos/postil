//! The Agent Client Protocol client: starts a coding agent, opens a session in the
//! repository, sends it prompts, and passes on what it reports.
//!
//! ACP is JSON-RPC 2.0, one JSON object per line, over the agent's standard input and output
//! (docs/design.md, "ACP integration"). The app is the client and speaks version 1. It
//! advertises form elicitation and no file or terminal capability, so the requests an agent
//! sends that the app serves are permission requests (`session/request_permission`) and
//! questions (`elicitation/create` or `session/create_elicitation`).
//!
//! # Threads
//!
//! An [`Agent`] has three threads. The reader and the stderr thread end when its process
//! exits; the writer ends when the [`Agent`] is dropped, or at its first failed write:
//!
//! - the writer sends the queued lines to standard input, so no caller blocks on an agent
//!   that stopped reading. A failed write means the agent closed its standard input, and
//!   nothing can reach it any more, even if it runs on: the writer marks it gone and kills
//!   its process group, so the reader reports the exit and a turn under way ends. A write
//!   fails only once every process holding the pipe's other end closed it: when `npx`
//!   keeps it open while the `node` it started closes its own, writes still fill the pipe,
//!   and the turn waits until Stop ends the agent;
//! - the reader reads standard output, hands each reply to whoever waits for it, and turns
//!   everything else into an [`Event`] for the callback given to [`Agent::spawn`];
//! - the stderr thread keeps the end of standard error for error messages, and keeps the
//!   agent from blocking on a full pipe.
//!
//! The caller's thread and the reader share one lock, over the requests in flight in both
//! directions. Nothing is called while it is held: the event callback runs after it is
//! released.
//!
//! Dropping the [`Agent`] marks it closed and kills its process group; the reader then
//! delivers no more events, and the threads end as the pipes close. An agent whose session
//! is not open yet ([`connection::Starting`]) is stopped from another thread by its
//! [`Stopper`].
//!
//! # Permission requests
//!
//! [`Agent`] numbers the permission requests and questions it passes on as events (0, 1, ...
//! across its turns, one count for both) and keeps the only table of them, with the agent's
//! own request id and, for a permission request, the options it offered; callers answer by
//! number. A request stays open until it is answered or cancelled, its turn ends, the agent
//! withdraws it (`$/cancel_request`), or the agent exits. While a cancel is under way, a
//! request is answered `cancelled` (a question: `cancel`) as it arrives and gets no number.
//! An unreadable request, or a question that is not a form, is answered with an error and
//! gets no number either.

mod connection;
mod wire;

pub(crate) use crate::acp::connection::{
    Agent, Event, Launch, PermissionRequest, Stopper, TurnEnd,
};
pub(crate) use crate::acp::wire::{
    ContentBlock, ElicitationReply, Location, PermissionKind, PermissionOption, PlanEntryStatus,
    SessionUpdate, StopReason, ToolCallContent, ToolCallStatus, ToolKind,
};
