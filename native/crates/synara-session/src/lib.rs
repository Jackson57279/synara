//! One logical connection to one Synara host.
//!
//! [`Session`] negotiates, connects, and reconnects with backoff. While
//! connected it keeps the shell stream and every open thread's stream
//! subscribed, resuming threads from their cursors. A change in
//! `serverInstanceId` (server restarted) drops every cursor so the next
//! subscription snapshots.
//!
//! The UI reads [`SessionState`] through [`Session::state`] and wakes on
//! [`SessionEvent`]s; the lock is never held across an await.

mod backoff;
mod session;

pub use backoff::Backoff;
pub use session::{ConnectionState, SendError, Session, SessionConfig, SessionEvent, SessionState};
